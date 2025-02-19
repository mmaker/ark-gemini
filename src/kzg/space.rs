//! Space-efficient implementation of the polynomial commitment of Kate et al.
use ark_ec::pairing::Pairing;
use ark_ec::scalar_mul::variable_base::{ChunkedPippenger, HashMapPippenger};
use ark_ec::CurveGroup;
use ark_ec::VariableBaseMSM;
use ark_ff::{PrimeField, Zero};
use ark_poly::Polynomial;
use ark_std::borrow::Borrow;
use ark_std::collections::VecDeque;
use ark_std::vec::Vec;

use crate::iterable::{Iterable, Reverse};
use crate::kzg::vanishing_polynomial;
use crate::misc::ceil_div;
use crate::subprotocols::sumcheck::streams::FoldedPolynomialTree;

use super::{time::CommitterKey, VerifierKey};
use super::{Commitment, EvaluationProof};

const LENGTH_MISMATCH_MSG: &str = "Expecting at least one element in the committer key.";

/// Steaming multi-scalar multiplication algorithm with hard-coded chunk size.
pub fn msm_chunks<G, F, I: ?Sized, J>(bases_stream: &J, scalars_stream: &I) -> G
where
    G: CurveGroup<ScalarField = F>,
    I: Iterable,
    F: PrimeField,
    I::Item: Borrow<F>,
    J: Iterable,
    J::Item: Borrow<G::Affine>,
{
    assert!(scalars_stream.len() <= bases_stream.len());

    // remove offset
    let mut bases = bases_stream.iter();
    let mut scalars = scalars_stream.iter();

    // align the streams
    bases
        .advance_by(bases_stream.len() - scalars_stream.len())
        .expect("bases not long enough");
    let step: usize = 1 << 20;
    let mut result = G::zero();
    for _ in 0..(scalars_stream.len() + step - 1) / step {
        let bases_step = (&mut bases)
            .take(step)
            .map(|b| *b.borrow())
            .collect::<Vec<_>>();
        let scalars_step = (&mut scalars)
            .take(step)
            .map(|s| *s.borrow())
            .collect::<Vec<_>>();
        result += G::msm(bases_step.as_slice(), scalars_step.as_slice());
    }
    result
}

/// The streaming SRS for the polynomial commitment scheme consists of the stream of consecutive powers of $G$.
#[derive(Clone)]
pub struct CommitterKeyStream<E, SG>
where
    E: Pairing,
    SG: Iterable,
    SG::Item: Borrow<E::G1Affine>,
{
    /// Stream of G1 elements.
    pub powers_of_g: SG,
    /// Two G2 elements needed for the committer.
    pub powers_of_g2: Vec<E::G2Affine>,
}

impl<E, SG> CommitterKeyStream<E, SG>
where
    E: Pairing,
    SG: Iterable,
    SG::Item: Borrow<E::G1Affine>,
{
    /// Turn a streaming SRS into a normal SRS.
    pub fn as_committer_key(&self, max_degree: usize) -> CommitterKey<E> {
        let offset = self.powers_of_g.len() - max_degree;
        let mut powers_of_g = self
            .powers_of_g
            .iter()
            .skip(offset)
            .map(|x| *x.borrow())
            .collect::<Vec<_>>();
        powers_of_g.reverse();
        let powers_of_g2 = self.powers_of_g2.clone().to_vec();
        CommitterKey {
            powers_of_g,
            powers_of_g2,
        }
    }

    /// Evaluate a single polynomial at the point `alpha`, and provide an evaluation proof along with the evaluation.
    pub fn open<SF>(
        &self,
        polynomial: &SF,
        alpha: &E::ScalarField,
        max_msm_buffer: usize,
    ) -> (E::ScalarField, EvaluationProof<E>)
    where
        SF: Iterable,
        SF::Item: Borrow<E::ScalarField>,
    {
        let mut quotient = ChunkedPippenger::<E::G1>::new(max_msm_buffer);

        let mut bases = self.powers_of_g.iter();
        let scalars = polynomial.iter();

        // align the streams and remove one degree
        bases
            .advance_by(self.powers_of_g.len() - polynomial.len())
            .expect(LENGTH_MISMATCH_MSG);

        let mut previous = E::ScalarField::zero();
        for (scalar, base) in scalars.zip(bases) {
            quotient.add(base, previous.into_bigint());
            let coefficient = previous * alpha + scalar.borrow();
            previous = coefficient;
        }

        let evaluation = previous;
        let evaluation_proof = quotient.finalize();
        (evaluation, EvaluationProof(evaluation_proof))
    }

    /// Evaluate a single polynomial at a set of points `points`, and provide an evaluation proof along with evaluations.
    pub fn open_multi_points<SF>(
        &self,
        polynomial: &SF,
        points: &[E::ScalarField],
        max_msm_buffer: usize,
    ) -> (Vec<E::ScalarField>, EvaluationProof<E>)
    where
        SF: Iterable,
        SF::Item: Borrow<E::ScalarField>,
    {
        let zeros = vanishing_polynomial(points);
        let mut quotient = ChunkedPippenger::<E::G1>::new(max_msm_buffer);
        let mut bases = self.powers_of_g.iter();
        bases
            .advance_by(self.powers_of_g.len() - polynomial.len() + zeros.degree())
            .unwrap();

        let mut state = VecDeque::<E::ScalarField>::with_capacity(points.len());

        let mut polynomial_iterator = polynomial.iter();

        (0..points.len()).for_each(|_| {
            state.push_back(*polynomial_iterator.next().unwrap().borrow());
        });

        for coefficient in polynomial_iterator {
            let coefficient = coefficient.borrow();
            let quotient_coefficient = state.pop_front().unwrap();
            state.push_back(*coefficient);
            (0..points.len()).for_each(|i| {
                state[i] -= zeros.coeffs[zeros.degree() - i - 1] * quotient_coefficient;
            });
            let base = bases.next().unwrap();
            quotient.add(base, quotient_coefficient.into_bigint());
        }
        let remainder = state.make_contiguous().to_vec();
        let commitment = EvaluationProof(quotient.finalize());
        (remainder, commitment)
    }

    /// The commitment procedures, that takes as input a committer key and the streaming coefficients of polynomial, and produces the desired commitment.
    pub fn commit<SF: ?Sized>(&self, polynomial: &SF) -> Commitment<E>
    where
        SF: Iterable,
        SF::Item: Borrow<E::ScalarField>,
    {
        assert!(self.powers_of_g.len() >= polynomial.len());

        Commitment(msm_chunks(&self.powers_of_g, polynomial))
    }

    pub fn batch_commit<'a, F>(
        &self,
        polynomials: &[&'a dyn Iterable<Item = F, Iter = &mut dyn Iterator<Item = F>>],
    ) -> Vec<Commitment<E>>
    where
        F: Borrow<E::ScalarField>,
    {
        polynomials.iter().map(|&p| self.commit(p)).collect()
    }

    /// The commitment procedures for our tensor check protocol.
    /// The algorithm takes advantage of the tree structure of folding polynomials in our protocol. Please refer to our paper for more details.
    /// The function takes as input a committer key and the tree structure of all the folding polynomials, and produces the desired commitment for each polynomial.
    pub fn commit_folding<SF>(
        &self,
        polynomials: &FoldedPolynomialTree<'_, E::ScalarField, SF>,
        max_msm_buffer: usize,
    ) -> Vec<Commitment<E>>
    where
        SF: Iterable,
        SF::Item: Borrow<E::ScalarField>,
    {
        let n = polynomials.depth();
        let mut pippengers: Vec<ChunkedPippenger<E::G1>> = Vec::new();
        let mut folded_bases = Vec::new();
        for i in 1..n + 1 {
            let pippenger = ChunkedPippenger::with_size(max_msm_buffer / n);
            let mut bases = self.powers_of_g.iter();

            let delta = self.powers_of_g.len() - ceil_div(polynomials.len(), 1 << i);
            bases.advance_by(delta).expect(LENGTH_MISMATCH_MSG);
            folded_bases.push(bases);
            pippengers.push(pippenger);
        }

        for (i, coefficient) in polynomials.iter() {
            let base = folded_bases[i - 1].next().unwrap();
            pippengers[i - 1].add(base, coefficient.into_bigint());
        }

        pippengers
            .into_iter()
            .map(|p| Commitment(p.finalize()))
            .collect::<Vec<_>>()
    }

    /// The commitment procedures for our tensor check protocol.
    /// The algorithm takes advantage of the tree structure of folding polynomials in our protocol. Please refer to our paper for more details.
    /// The function evaluates all the folding polynomials at a set of evaluation points `points` and produces a single batched evaluation proof.
    /// `eta` is the random challenge for batching folding polynomials.
    pub fn open_folding<'a, SF>(
        &self,
        polynomials: FoldedPolynomialTree<'a, E::ScalarField, SF>,
        points: &[E::ScalarField],
        etas: &[E::ScalarField],
        max_msm_buffer: usize,
    ) -> (Vec<Vec<E::ScalarField>>, EvaluationProof<E>)
    where
        SG: Iterable,
        SF: Iterable,
        E: Pairing,
        SG::Item: Borrow<E::G1Affine>,
        SF::Item: Borrow<E::ScalarField> + Copy,
    {
        let n = polynomials.depth();
        let mut pippenger = HashMapPippenger::<E::G1>::new(max_msm_buffer);
        let mut folded_bases = Vec::new();
        let zeros = vanishing_polynomial(points);
        let mut remainders = vec![VecDeque::new(); n];

        for i in 1..n + 1 {
            let mut bases = self.powers_of_g.iter();
            let delta = self.powers_of_g.len() - ceil_div(polynomials.len(), 1 << i);
            bases.advance_by(delta).expect(LENGTH_MISMATCH_MSG);

            (0..points.len()).for_each(|_| {
                remainders[i - 1].push_back(E::ScalarField::zero());
            });

            folded_bases.push(bases);
        }

        for (i, coefficient) in polynomials.iter() {
            if i == 0 {
                continue;
            } // XXX. skip the 0th elements automatically

            let base = folded_bases[i - 1].next().unwrap();
            let coefficient = coefficient.borrow();
            let quotient_coefficient = remainders[i - 1].pop_front().unwrap();
            remainders[i - 1].push_back(*coefficient);
            (0..points.len()).for_each(|j| {
                remainders[i - 1][j] -= zeros.coeffs[zeros.degree() - j - 1] * quotient_coefficient;
            });

            let scalar = etas[i - 1] * quotient_coefficient;
            pippenger.add(base, scalar);
        }

        let evaluation_proof = pippenger.finalize();
        let remainders = remainders
            .iter_mut()
            .map(|x| x.make_contiguous().to_vec())
            .collect::<Vec<_>>();

        (remainders, EvaluationProof(evaluation_proof))
    }
}

impl<'a, E: Pairing> From<&'a CommitterKey<E>>
    for CommitterKeyStream<E, Reverse<&'a [E::G1Affine]>>
{
    fn from(ck: &'a CommitterKey<E>) -> Self {
        CommitterKeyStream {
            powers_of_g: Reverse(ck.powers_of_g.as_slice()),
            powers_of_g2: ck.powers_of_g2.clone(),
        }
    }
}

impl<E, SG> From<&CommitterKeyStream<E, SG>> for VerifierKey<E>
where
    E: Pairing,
    SG: Iterable,
    SG::Item: Borrow<E::G1Affine>,
{
    fn from(ck: &CommitterKeyStream<E, SG>) -> Self {
        let powers_of_g2 = ck.powers_of_g2.to_vec();
        // take the first element from the stream
        let g = *ck
            .powers_of_g
            .iter()
            .last()
            .expect(LENGTH_MISMATCH_MSG)
            .borrow();
        Self {
            powers_of_g2,
            powers_of_g: vec![g],
        }
    }
}

#[test]
fn test_open_multi_points() {
    use crate::ark_std::UniformRand;
    use crate::misc::evaluate_be;
    use ark_bls12_381::{Bls12_381, Fr};
    use ark_ff::Field;
    use ark_poly::univariate::DensePolynomial;
    use ark_poly::DenseUVPolynomial;
    use ark_std::test_rng;

    let max_msm_buffer = 1 << 20;
    let rng = &mut test_rng();
    // f = 80*x^6 + 80*x^5 + 88*x^4 + 3*x^3 + 73*x^2 + 7*x + 24
    let polynomial = [
        Fr::from(80u64),
        Fr::from(80u64),
        Fr::from(88u64),
        Fr::from(3u64),
        Fr::from(73u64),
        Fr::from(7u64),
        Fr::from(24u64),
    ];
    let polynomial_stream = &polynomial[..];
    let beta = Fr::from(53u64);

    let time_ck = CommitterKey::<Bls12_381>::new(200, 3, rng);
    let space_ck = CommitterKeyStream::from(&time_ck);

    let (remainder, _commitment) = space_ck.open_multi_points(
        &polynomial_stream,
        &[beta.square(), beta, -beta],
        max_msm_buffer,
    );
    let evaluation_remainder = evaluate_be(&remainder, &beta);
    assert_eq!(evaluation_remainder, Fr::from(1807299544171u64));

    let (remainder, _commitment) =
        space_ck.open_multi_points(&polynomial_stream, &[beta], max_msm_buffer);
    assert_eq!(remainder.len(), 1);

    // get a random polynomial with random coefficient,
    let polynomial = DensePolynomial::rand(100, rng).coeffs().to_vec();
    let polynomial_stream = &polynomial[..];
    let beta = Fr::rand(rng);
    let (_, evaluation_proof_batch) =
        space_ck.open_multi_points(&polynomial_stream, &[beta], max_msm_buffer);
    let (_, evaluation_proof_single) = space_ck.open(&polynomial_stream, &beta, max_msm_buffer);
    assert_eq!(evaluation_proof_batch, evaluation_proof_single);

    let (remainder, _evaluation_poof) = space_ck.open_multi_points(
        &polynomial_stream,
        &[beta, -beta, beta.square()],
        max_msm_buffer,
    );
    let expected_evaluation = evaluate_be(&remainder, &beta);
    let obtained_evaluation = evaluate_be(&polynomial, &beta);
    assert_eq!(expected_evaluation, obtained_evaluation);
    let expected_evaluation = evaluate_be(&remainder, &beta.square());
    let obtained_evaluation = evaluate_be(&polynomial, &beta.square());
    assert_eq!(expected_evaluation, obtained_evaluation);
    // let expected_evaluation = evaluate_be(&remainder, &beta.square());
    // let obtained_evaluation = evaluate_be(&polynomial, &beta.square());
    // assert_eq!(expected_evaluation, obtained_evaluation);
    // let expected_evaluation = evaluate_be(&remainder, &beta.square());
    // let obtained_evaluation = evaluate_be(&polynomial, &beta.square());
    // assert_eq!(expected_evaluation, obtained_evaluation);
}
