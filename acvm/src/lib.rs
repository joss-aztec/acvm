// Key is currently {NPComplete_lang}_{OptionalFanIn}_ProofSystem_OrgName
// Org name is needed because more than one implementation of the same proof system may arise

pub mod compiler;
pub mod pwg;

use std::collections::BTreeMap;

use acir::{
    circuit::{directives::Directive, gate::GadgetCall, Circuit, Gate},
    native_types::{Expression, Witness},
    OPCODE,
};

use crate::pwg::{arithmetic::ArithmeticSolver, logic::LogicSolver};
use num_bigint::BigUint;
use num_traits::{One, Zero};

// re-export acir
pub use acir;
pub use acir::FieldElement;

#[derive(PartialEq, Eq, Debug)]
pub enum GateResolution {
    Resolved,                  //Gate is solved
    Skip,                      //Gate cannot be solved
    UnknownError(String),      //Generic error
    UnsupportedOpcode(OPCODE), //Unsupported Opcode
    UnsatisfiedConstrain,      //Gate is not satisfied
}

pub trait Backend: SmartContract + ProofSystemCompiler + PartialWitnessGenerator {}

/// This component will generate the backend specific output for
/// each OPCODE.
/// Returns an Error if the backend does not support that OPCODE
pub trait PartialWitnessGenerator {
    fn solve(
        &self,
        initial_witness: &mut BTreeMap<Witness, FieldElement>,
        gates: Vec<Gate>,
    ) -> GateResolution {
        if gates.is_empty() {
            return GateResolution::Resolved;
        }
        let mut unsolved_gates: Vec<Gate> = Vec::new();

        for gate in gates.into_iter() {
            let unsolved = match &gate {
                Gate::Arithmetic(arith) => {
                    let result = ArithmeticSolver::solve(initial_witness, arith);
                    match result {
                        GateResolution::Resolved => false,
                        GateResolution::Skip => true,
                        _ => return result,
                    }
                }
                Gate::GadgetCall(gc) if gc.name == OPCODE::RANGE => {
                    // TODO: this consistency check can be moved to a general function
                    let defined_input_size = OPCODE::RANGE
                        .definition()
                        .input_size
                        .fixed_size()
                        .expect("infallible: input for range gate is fixed");

                    if gc.inputs.len() != defined_input_size as usize {
                        return GateResolution::UnknownError(
                            "defined input size does not equal given input size".to_string(),
                        );
                    }

                    // For the range constraint, we know that the input size should be one
                    assert_eq!(defined_input_size, 1);

                    let input = gc
                        .inputs
                        .first()
                        .expect("infallible: checked that input size is 1");

                    if let Some(w_value) = initial_witness.get(&input.witness) {
                        if w_value.num_bits() > input.num_bits {
                            return GateResolution::UnsatisfiedConstrain;
                        }
                        false
                    } else {
                        true
                    }
                }
                Gate::GadgetCall(gc) if gc.name == OPCODE::AND => {
                    !LogicSolver::solve_and_gate(initial_witness, gc)
                }
                Gate::GadgetCall(gc) if gc.name == OPCODE::XOR => {
                    !LogicSolver::solve_xor_gate(initial_witness, gc)
                }
                Gate::GadgetCall(gc) => {
                    let mut unsolvable = false;
                    for i in &gc.inputs {
                        if !initial_witness.contains_key(&i.witness) {
                            unsolvable = true;
                            break;
                        }
                    }
                    if unsolvable {
                        true
                    } else if let Err(op) = Self::solve_gadget_call(initial_witness, gc) {
                        return GateResolution::UnsupportedOpcode(op);
                    } else {
                        false
                    }
                }
                Gate::Directive(directive) => match directive {
                    Directive::Invert { x, result } => match initial_witness.get(x) {
                        None => true,
                        Some(val) => {
                            let inverse = val.inverse();
                            initial_witness.insert(*result, inverse);
                            false
                        }
                    },
                    Directive::Quotient {
                        a,
                        b,
                        q,
                        r,
                        predicate,
                    } => {
                        match (
                            Self::get_value(a, initial_witness),
                            Self::get_value(b, initial_witness),
                        ) {
                            (Some(val_a), Some(val_b)) => {
                                let int_a = BigUint::from_bytes_be(&val_a.to_bytes());
                                let int_b = BigUint::from_bytes_be(&val_b.to_bytes());
                                let default = Box::new(Expression::one());
                                let pred = predicate.as_ref().unwrap_or(&default);
                                if let Some(pred_value) = Self::get_value(pred, initial_witness) {
                                    let (int_r, int_q) = if pred_value.is_zero() {
                                        (BigUint::zero(), BigUint::zero())
                                    } else {
                                        (&int_a % &int_b, &int_a / &int_b)
                                    };
                                    initial_witness.insert(
                                        *q,
                                        FieldElement::from_be_bytes_reduce(&int_q.to_bytes_be()),
                                    );
                                    initial_witness.insert(
                                        *r,
                                        FieldElement::from_be_bytes_reduce(&int_r.to_bytes_be()),
                                    );
                                    false
                                } else {
                                    true
                                }
                            }
                            _ => true,
                        }
                    }
                    Directive::Truncate { a, b, c, bit_size } => match initial_witness.get(a) {
                        Some(val_a) => {
                            let pow: BigUint = BigUint::one() << bit_size;

                            let int_a = BigUint::from_bytes_be(&val_a.to_bytes());
                            let int_b: BigUint = &int_a % &pow;
                            let int_c: BigUint = (&int_a - &int_b) / &pow;

                            initial_witness.insert(
                                *b,
                                FieldElement::from_be_bytes_reduce(&int_b.to_bytes_be()),
                            );
                            initial_witness.insert(
                                *c,
                                FieldElement::from_be_bytes_reduce(&int_c.to_bytes_be()),
                            );
                            false
                        }
                        _ => true,
                    },
                    Directive::Split { a, b, bit_size } => {
                        match Self::get_value(a, initial_witness) {
                            Some(val_a) => {
                                let a_big = BigUint::from_bytes_be(&val_a.to_bytes());
                                for i in 0..*bit_size {
                                    let j = i as usize;
                                    let v = if a_big.bit(j as u64) {
                                        FieldElement::one()
                                    } else {
                                        FieldElement::zero()
                                    };
                                    match initial_witness.entry(b[j]) {
                                        std::collections::btree_map::Entry::Vacant(e) => {
                                            e.insert(v);
                                        }
                                        std::collections::btree_map::Entry::Occupied(e) => {
                                            if e.get() != &v {
                                                return GateResolution::UnsatisfiedConstrain;
                                            }
                                        }
                                    }
                                }
                                false
                            }
                            _ => true,
                        }
                    }
                    Directive::ToBytes { a, b, byte_size } => {
                        match Self::get_value(a, initial_witness) {
                            Some(val_a) => {
                                let mut a_bytes = val_a.to_bytes();
                                a_bytes.reverse();
                                for i in 0..*byte_size {
                                    let i_usize = i as usize;
                                    let v = FieldElement::from_be_bytes_reduce(&[a_bytes[i_usize]]);
                                    match initial_witness.entry(b[i_usize]) {
                                        std::collections::btree_map::Entry::Vacant(e) => {
                                            e.insert(v);
                                        }
                                        std::collections::btree_map::Entry::Occupied(e) => {
                                            if e.get() != &v {
                                                return GateResolution::UnsatisfiedConstrain;
                                            }
                                        }
                                    }
                                }
                                false
                            }
                            _ => true,
                        }
                    }
                    Directive::Oddrange { a, b, r, bit_size } => match initial_witness.get(a) {
                        Some(val_a) => {
                            let int_a = BigUint::from_bytes_be(&val_a.to_bytes());
                            let pow: BigUint = BigUint::one() << (bit_size - 1);
                            if int_a >= (&pow << 1) {
                                return GateResolution::UnsatisfiedConstrain;
                            }
                            let bb = &int_a & &pow;
                            let int_r = &int_a - &bb;
                            let int_b = &bb >> (bit_size - 1);

                            initial_witness.insert(
                                *b,
                                FieldElement::from_be_bytes_reduce(&int_b.to_bytes_be()),
                            );
                            initial_witness.insert(
                                *r,
                                FieldElement::from_be_bytes_reduce(&int_r.to_bytes_be()),
                            );
                            false
                        }
                        _ => true,
                    },
                },
            };
            if unsolved {
                unsolved_gates.push(gate);
            }
        }
        self.solve(initial_witness, unsolved_gates)
    }

    fn solve_gadget_call(
        initial_witness: &mut BTreeMap<Witness, FieldElement>,
        gc: &GadgetCall,
    ) -> Result<(), OPCODE>;

    fn get_value(
        a: &Expression,
        initial_witness: &std::collections::BTreeMap<Witness, FieldElement>,
    ) -> Option<FieldElement> {
        let mut result = a.q_c;
        for i in &a.linear_combinations {
            if let Some(f) = initial_witness.get(&i.1) {
                result += i.0 * *f;
            } else {
                return None;
            }
        }
        for i in &a.mul_terms {
            if let (Some(f), Some(g)) = (initial_witness.get(&i.1), initial_witness.get(&i.2)) {
                result += i.0 * *f * *g;
            } else {
                return None;
            }
        }
        Some(result)
    }
}

pub trait SmartContract {
    // Takes a verification  key and produces a smart contract
    // The platform indicator allows a backend to support multiple smart contract platforms
    //
    // fn verification_key(&self, platform: u8, vk: &[u8]) -> &[u8] {
    //     todo!("currently the backend is not configured to use this.")
    // }

    /// Takes an ACIR circuit, the number of witnesses and the number of public inputs
    /// Then returns an Ethereum smart contract
    ///
    /// XXX: This will be deprecated in future releases for the above method.
    /// This deprecation may happen in two stages:
    /// The first stage will remove `num_witnesses` and `num_public_inputs` parameters.
    /// If we cannot avoid `num_witnesses`, it can be added into the Circuit struct.
    fn eth_contract_from_cs(&self, circuit: Circuit) -> String;
}

pub trait ProofSystemCompiler {
    /// The NPC language that this proof system directly accepts.
    /// It is possible for ACVM to transpile to different languages, however it is advised to create a new backend
    /// as this in most cases will be inefficient. For this reason, we want to throw a hard error
    /// if the language and proof system does not line up.
    fn np_language(&self) -> Language;

    /// Creates a Proof given the circuit description and the witness values.
    /// It is important to note that the intermediate witnesses for blackbox functions will not generated
    /// This is the responsibility of the proof system.
    ///
    /// See `SmartContract` regarding the removal of `num_witnesses` and `num_public_inputs`
    fn prove_with_meta(
        &self,
        circuit: Circuit,
        witness_values: BTreeMap<Witness, FieldElement>,
    ) -> Vec<u8>;

    /// Verifies a Proof, given the circuit description.
    ///
    /// XXX: This will be changed in the future to accept a VerifierKey.
    /// At the moment, the Aztec backend API only accepts a constraint system,
    /// which is why this is here.
    ///
    /// See `SmartContract` regarding the removal of `num_witnesses` and `num_public_inputs`
    fn verify_from_cs(
        &self,
        proof: &[u8],
        public_input: Vec<FieldElement>,
        circuit: Circuit,
    ) -> bool;

    fn get_exact_circuit_size(&self, circuit: Circuit) -> u32;
}

/// Supported NP complete languages
/// This might need to be in ACIR instead
#[derive(Debug, Clone)]
pub enum Language {
    R1CS,
    PLONKCSat { width: usize },
}
// TODO: We can remove this and have backends simply say what opcodes they support
pub trait CustomGate {
    fn supports(&self, opcode: &str) -> bool;
    fn supports_gate(&self, gate: &Gate) -> bool;
}

impl CustomGate for Language {
    fn supports(&self, _opcode: &str) -> bool {
        match self {
            Language::R1CS => false,
            Language::PLONKCSat { .. } => true,
        }
    }

    // TODO: document this method, its intentions are not clear
    // TODO: it was made to copy the functionality of the matches
    // TODO code that was there before
    fn supports_gate(&self, gate: &Gate) -> bool {
        let is_supported_gate = match gate {
            Gate::GadgetCall(gc) if gc.name == OPCODE::RANGE => true,
            Gate::GadgetCall(gc) if gc.name == OPCODE::AND => true,
            Gate::GadgetCall(gc) if gc.name == OPCODE::XOR => true,
            Gate::GadgetCall(_) | Gate::Arithmetic(_) | Gate::Directive(_) => false,
        };

        let is_r1cs = match self {
            Language::R1CS => true,
            Language::PLONKCSat { .. } => false,
        };

        !(is_supported_gate | is_r1cs)
    }
}

pub fn hash_constraint_system(cs: &Circuit) {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(&format!("{:?}", cs));
    let result = hasher.finalize();
    println!("hash of constraint system : {:x?}", &result[..]);
}