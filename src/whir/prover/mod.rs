use std::ops::Deref;

use p3_commit::{ExtensionMmcs, Mmcs};
use p3_dft::TwoAdicSubgroupDft;
use p3_field::{ExtensionField, Field, PrimeField64, TwoAdicField};
use p3_matrix::dense::RowMajorMatrix;
use p3_merkle_tree::MerkleTreeMmcs;
use p3_symmetric::{CryptographicHasher, PseudoCompressionFunction};
use round::RoundState;
use serde::{Deserialize, Serialize};

use super::{committer::Witness, parameters::WhirConfig, statement::Statement};
use crate::{
    fiat_shamir::{errors::ProofResult, pow::traits::PowStrategy, prover::ProverState},
    poly::{
        coeffs::{CoefficientList, CoefficientStorage},
        multilinear::MultilinearPoint,
    },
    sumcheck::sumcheck_single::SumcheckSingle,
    utils::expand_randomness,
    whir::{
        parameters::RoundConfig,
        prover::proof::WhirProof,
        statement::Weights,
        utils::{get_challenge_stir_queries, sample_ood_points},
    },
};

pub mod proof;
pub mod round;

pub type Proof<const DIGEST_ELEMS: usize> = Vec<Vec<[u8; DIGEST_ELEMS]>>;
pub type Leafs<F> = Vec<Vec<F>>;

#[derive(Debug)]
pub struct Prover<EF, F, H, C, PowStrategy>(pub WhirConfig<EF, F, H, C, PowStrategy>)
where
    F: Field + TwoAdicField + PrimeField64,
    EF: ExtensionField<F> + TwoAdicField;

impl<EF, F, H, C, PS> Deref for Prover<EF, F, H, C, PS>
where
    F: Field + TwoAdicField + PrimeField64,
    EF: ExtensionField<F> + TwoAdicField,
{
    type Target = WhirConfig<EF, F, H, C, PS>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<EF, F, H, C, PS> Prover<EF, F, H, C, PS>
where
    F: Field + TwoAdicField + PrimeField64,
    EF: ExtensionField<F> + TwoAdicField,
    PS: PowStrategy,
{
    fn validate_parameters(&self) -> bool {
        self.mv_parameters.num_variables
            == self.folding_factor.total_number(self.n_rounds()) + self.final_sumcheck_rounds
    }

    fn validate_statement(&self, statement: &Statement<EF>) -> bool {
        statement.num_variables() == self.mv_parameters.num_variables
            && (self.initial_statement || statement.constraints.is_empty())
    }

    fn validate_witness<const DIGEST_ELEMS: usize>(
        &self,
        witness: &Witness<EF, F, DIGEST_ELEMS>,
    ) -> bool {
        assert_eq!(witness.ood_points.len(), witness.ood_answers.len());
        if !self.initial_statement {
            assert!(witness.ood_points.is_empty());
        }
        witness.polynomial.num_variables() == self.mv_parameters.num_variables
    }

    pub fn prove<D, const DIGEST_ELEMS: usize>(
        &self,
        dft: &D,
        prover_state: &mut ProverState<EF, F>,
        statement: Statement<EF>,
        witness: Witness<EF, F, DIGEST_ELEMS>,
    ) -> ProofResult<WhirProof<F, EF, DIGEST_ELEMS>>
    where
        H: CryptographicHasher<F, [u8; DIGEST_ELEMS]> + Sync,
        C: PseudoCompressionFunction<[u8; DIGEST_ELEMS], 2> + Sync,
        [u8; DIGEST_ELEMS]: Serialize + for<'de> Deserialize<'de>,
        D: TwoAdicSubgroupDft<F>,
    {
        // Validate parameters
        assert!(
            self.validate_parameters()
                && self.validate_statement(&statement)
                && self.validate_witness(&witness)
        );

        let mut round_state =
            RoundState::initialize_first_round_state(self, prover_state, statement, witness)?;

        // Run WHIR rounds
        for round in 0..=self.n_rounds() {
            self.round(round, dft, prover_state, &mut round_state)?;
        }

        // Extract WhirProof
        round_state.randomness_vec.reverse();
        let statement_values_at_random_point = round_state
            .statement
            .constraints
            .iter()
            .filter_map(|(weights, _)| {
                if let Weights::Linear { weight } = weights {
                    Some(
                        weight
                            .eval_extension(&MultilinearPoint(round_state.randomness_vec.clone())),
                    )
                } else {
                    None
                }
            })
            .collect();

        Ok(WhirProof {
            commitment_merkle_paths: round_state.commitment_merkle_proof.unwrap(),
            merkle_paths: round_state.merkle_proofs,
            statement_values_at_random_point,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn round<D, const DIGEST_ELEMS: usize>(
        &self,
        round_index: usize,
        dft: &D,
        prover_state: &mut ProverState<EF, F>,
        round_state: &mut RoundState<EF, F, DIGEST_ELEMS>,
    ) -> ProofResult<()>
    where
        H: CryptographicHasher<F, [u8; DIGEST_ELEMS]> + Sync,
        C: PseudoCompressionFunction<[u8; DIGEST_ELEMS], 2> + Sync,
        [u8; DIGEST_ELEMS]: Serialize + for<'de> Deserialize<'de>,
        D: TwoAdicSubgroupDft<F>,
    {
        // Fold the coefficients
        let folded_coefficients = round_state
            .coefficients
            .fold(&round_state.folding_randomness);

        let num_variables =
            self.mv_parameters.num_variables - self.folding_factor.total_number(round_index);
        // num_variables should match the folded_coefficients here.
        assert_eq!(num_variables, folded_coefficients.num_variables());

        // Base case: final round reached
        if round_index == self.n_rounds() {
            return self.final_round(round_index, prover_state, round_state, &folded_coefficients);
        }

        let round_params = &self.round_parameters[round_index];

        // Compute the folding factors for later use
        let folding_factor_next = self.folding_factor.at_round(round_index + 1);

        // Compute polynomial evaluations and build Merkle tree
        let new_domain = round_state.domain.scale(2);
        let expansion = new_domain.size() / folded_coefficients.num_coeffs();
        let folded_matrix = {
            let mut coeffs = folded_coefficients.coeffs().to_vec();
            coeffs.resize(coeffs.len() * expansion, EF::ZERO);
            // Do DFT on only interleaved polys to be folded.
            dft.dft_algebra_batch(RowMajorMatrix::new(coeffs, 1 << folding_factor_next))
        };

        let mmcs = MerkleTreeMmcs::new(self.merkle_hash.clone(), self.merkle_compress.clone());
        let extension_mmcs = ExtensionMmcs::new(mmcs.clone());
        let (root, prover_data) = extension_mmcs.commit_matrix(folded_matrix);

        // Observe Merkle root in challenger
        prover_state.add_digest(root)?;

        // Handle OOD (Out-Of-Domain) samples
        let (ood_points, ood_answers) = sample_ood_points(
            prover_state,
            round_params.ood_samples,
            num_variables,
            |point| folded_coefficients.evaluate(point),
        )?;

        // STIR Queries
        let (stir_challenges, stir_challenges_indexes) = self.compute_stir_queries(
            round_index,
            prover_state,
            round_state,
            num_variables,
            round_params,
            ood_points,
        )?;

        // Collect Merkle proofs for stir queries
        let stir_evaluations = match &round_state.merkle_prover_data {
            None => {
                let mut answers = Vec::with_capacity(stir_challenges_indexes.len());
                let mut merkle_proof = Vec::with_capacity(stir_challenges_indexes.len());
                for challenge in &stir_challenges_indexes {
                    let (commitment_leaf, commitment_root) =
                        mmcs.open_batch(*challenge, &round_state.commitment_merkle_prover_data);
                    answers.push(commitment_leaf[0].clone());
                    merkle_proof.push(commitment_root);
                }
                // Evaluate answers in the folding randomness.
                let mut stir_evaluations = ood_answers;
                // Exactly one growth
                stir_evaluations.reserve_exact(answers.len());

                for answer in &answers {
                    stir_evaluations.push(
                        CoefficientList::new(answer.clone())
                            .evaluate(&round_state.folding_randomness),
                    );
                }

                round_state.commitment_merkle_proof = Some((answers, merkle_proof));
                stir_evaluations
            }
            Some(data) => {
                let mut answers = Vec::with_capacity(stir_challenges_indexes.len());
                let mut merkle_proof = Vec::with_capacity(stir_challenges_indexes.len());
                for challenge in &stir_challenges_indexes {
                    let (leaf, proof) = extension_mmcs.open_batch(*challenge, data);
                    answers.push(leaf[0].clone());
                    merkle_proof.push(proof);
                }
                // Evaluate answers in the folding randomness.
                let mut stir_evaluations = ood_answers;
                // Exactly one growth
                stir_evaluations.reserve_exact(answers.len());

                for answer in &answers {
                    stir_evaluations.push(
                        CoefficientList::new(answer.clone())
                            .evaluate(&round_state.folding_randomness),
                    );
                }

                round_state.merkle_proofs.push((answers, merkle_proof));
                stir_evaluations
            }
        };

        // PoW
        if round_params.pow_bits > 0. {
            prover_state.challenge_pow::<PS>(round_params.pow_bits)?;
        }

        // Randomness for combination
        let [combination_randomness_gen] = prover_state.challenge_scalars()?;
        let combination_randomness =
            expand_randomness(combination_randomness_gen, stir_challenges.len());

        let mut sumcheck_prover =
            if let Some(mut sumcheck_prover) = round_state.sumcheck_prover.take() {
                sumcheck_prover.add_new_equality(
                    &stir_challenges,
                    &stir_evaluations,
                    &combination_randomness,
                );
                sumcheck_prover
            } else {
                let mut statement = Statement::new(folded_coefficients.num_variables());

                for (point, eval) in stir_challenges.into_iter().zip(stir_evaluations) {
                    let weights = Weights::evaluation(point);
                    statement.add_constraint(weights, eval);
                }

                SumcheckSingle::from_extension_coeffs(
                    folded_coefficients.clone(),
                    &statement,
                    combination_randomness[1],
                )
            };

        let folding_randomness = sumcheck_prover.compute_sumcheck_polynomials::<PS>(
            prover_state,
            folding_factor_next,
            round_params.folding_pow_bits,
        )?;

        let start_idx = self.folding_factor.total_number(round_index);
        let dst_randomness =
            &mut round_state.randomness_vec[start_idx..][..folding_randomness.0.len()];

        for (dst, src) in dst_randomness
            .iter_mut()
            .zip(folding_randomness.0.iter().rev())
        {
            *dst = *src;
        }

        // Update round state
        round_state.domain = new_domain;
        round_state.sumcheck_prover = Some(sumcheck_prover);
        round_state.folding_randomness = folding_randomness;
        round_state.coefficients = CoefficientStorage::Extension(folded_coefficients);
        round_state.merkle_prover_data = Some(prover_data);

        Ok(())
    }

    fn final_round<const DIGEST_ELEMS: usize>(
        &self,
        round_index: usize,
        prover_state: &mut ProverState<EF, F>,
        round_state: &mut RoundState<EF, F, DIGEST_ELEMS>,
        folded_coefficients: &CoefficientList<EF>,
    ) -> ProofResult<()>
    where
        H: CryptographicHasher<F, [u8; DIGEST_ELEMS]> + Sync,
        C: PseudoCompressionFunction<[u8; DIGEST_ELEMS], 2> + Sync,
        [u8; DIGEST_ELEMS]: Serialize + for<'de> Deserialize<'de>,
    {
        // Directly send coefficients of the polynomial to the verifier.
        prover_state.add_scalars(folded_coefficients.coeffs())?;
        // Final verifier queries and answers. The indices are over the folded domain.
        let final_challenge_indexes = get_challenge_stir_queries(
            // The size of the original domain before folding
            round_state.domain.size(),
            // The folding factor we used to fold the previous polynomial
            self.folding_factor.at_round(round_index),
            self.final_queries,
            prover_state,
        )?;

        // Every query requires opening these many in the previous Merkle tree
        let mmcs = MerkleTreeMmcs::new(self.merkle_hash.clone(), self.merkle_compress.clone());
        let extension_mmcs = ExtensionMmcs::new(mmcs.clone());

        match &round_state.merkle_prover_data {
            None => {
                let mut answers = Vec::with_capacity(final_challenge_indexes.len());
                let mut merkle_proof = Vec::with_capacity(final_challenge_indexes.len());

                for challenge in final_challenge_indexes {
                    let (commitment_leaf, commitment_root) =
                        mmcs.open_batch(challenge, &round_state.commitment_merkle_prover_data);
                    answers.push(commitment_leaf[0].clone());
                    merkle_proof.push(commitment_root);
                }

                round_state.commitment_merkle_proof = Some((answers, merkle_proof));
            }

            Some(data) => {
                let mut answers = Vec::with_capacity(final_challenge_indexes.len());
                let mut merkle_proof = Vec::with_capacity(final_challenge_indexes.len());
                for challenge in final_challenge_indexes {
                    let (leaf, proof) = extension_mmcs.open_batch(challenge, data);
                    answers.push(leaf[0].clone());
                    merkle_proof.push(proof);
                }
                round_state.merkle_proofs.push((answers, merkle_proof));
            }
        }

        // PoW
        if self.final_pow_bits > 0. {
            prover_state.challenge_pow::<PS>(self.final_pow_bits)?;
        }

        // Run final sumcheck if required
        if self.final_sumcheck_rounds > 0 {
            let final_folding_randomness = round_state
                .sumcheck_prover
                .clone()
                .unwrap_or_else(|| {
                    SumcheckSingle::from_extension_coeffs(
                        folded_coefficients.clone(),
                        &round_state.statement,
                        EF::ONE,
                    )
                })
                .compute_sumcheck_polynomials::<PS>(
                    prover_state,
                    self.final_sumcheck_rounds,
                    self.final_folding_pow_bits,
                )?;

            let start_idx = self.folding_factor.total_number(round_index);
            let rand_dst = &mut round_state.randomness_vec
                [start_idx..start_idx + final_folding_randomness.0.len()];

            for (dst, src) in rand_dst
                .iter_mut()
                .zip(final_folding_randomness.0.iter().rev())
            {
                *dst = *src;
            }
        }

        Ok(())
    }

    fn compute_stir_queries<const DIGEST_ELEMS: usize>(
        &self,
        round_index: usize,
        prover_state: &mut ProverState<EF, F>,
        round_state: &RoundState<EF, F, DIGEST_ELEMS>,
        num_variables: usize,
        round_params: &RoundConfig,
        ood_points: Vec<EF>,
    ) -> ProofResult<(Vec<MultilinearPoint<EF>>, Vec<usize>)> {
        let stir_challenges_indexes = get_challenge_stir_queries(
            round_state.domain.size(),
            self.folding_factor.at_round(round_index),
            round_params.num_queries,
            prover_state,
        )?;

        // Compute the generator of the folded domain, in the extension field
        let domain_scaled_gen = round_state
            .domain
            .backing_domain
            .element(1 << self.folding_factor.at_round(round_index));
        let stir_challenges = ood_points
            .into_iter()
            .chain(
                stir_challenges_indexes
                    .iter()
                    .map(|i| domain_scaled_gen.exp_u64(*i as u64)),
            )
            .map(|univariate| MultilinearPoint::expand_from_univariate(univariate, num_variables))
            .collect();

        Ok((stir_challenges, stir_challenges_indexes))
    }
}
