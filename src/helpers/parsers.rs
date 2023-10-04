use core::iter::Peekable;

use bitcoin::blockdata::opcodes::all::{OP_ENDIF, OP_IF};
use bitcoin::blockdata::script::{Instruction, Instructions};
use bitcoin::opcodes::OP_FALSE;
use bitcoin::{Script, Transaction};
use serde::{Deserialize, Serialize};

use super::{BODY_TAG, PUBLICKEY_TAG, RANDOM_TAG, ROLLUP_NAME_TAG, SIGNATURE_TAG};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParsedInscription {
    pub body: Vec<u8>,
    pub signature: Vec<u8>,
    pub public_key: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ParserError {
    InvalidRollupName,
    EnvelopeHasNonPushOp,
    EnvelopeHasIncorrectFormat,
    NonTapscriptWitness,
    IncorrectSignature,
}

pub fn parse_transaction(
    tx: &Transaction,
    rollup_name: &str,
) -> Result<ParsedInscription, ParserError> {
    let script = get_script(tx)?;
    let mut instructions = script.instructions().peekable();
    parse_relevant_inscriptions(&mut instructions, rollup_name)
}

// Returns the script from the first input of the transaction
fn get_script(tx: &Transaction) -> Result<&Script, ParserError> {
    tx.input[0]
        .witness
        .tapscript()
        .ok_or(ParserError::NonTapscriptWitness)
}

// TODO: discuss removing tags
// Parses the inscription from script if it is relevant to the rollup
fn parse_relevant_inscriptions(
    instructions: &mut Peekable<Instructions>,
    rollup_name: &str,
) -> Result<ParsedInscription, ParserError> {
    let mut last_op = None;
    let mut inside_envelope = false;
    let mut inside_envelope_index = 0;

    let mut body: Vec<u8> = Vec::new();
    let mut signature: Vec<u8> = Vec::new();
    let mut public_key: Vec<u8> = Vec::new();

    // this while loop is optimized for the least amount of iterations
    // for a strict envelope structure
    // nothing other than data pushes should be inside the envelope
    // the loop will break after the first envelope is parsed
    while let Some(Ok(instruction)) = instructions.next() {
        match instruction {
            Instruction::Op(OP_IF) => {
                if last_op == Some(OP_FALSE) {
                    inside_envelope = true;
                } else if inside_envelope {
                    return Err(ParserError::EnvelopeHasNonPushOp);
                }
            }
            Instruction::Op(OP_ENDIF) => {
                if inside_envelope {
                    break; // we are done parsing
                }
            }
            Instruction::Op(another_op) => {
                // don't allow anything except data pushes inside envelope
                if inside_envelope {
                    return Err(ParserError::EnvelopeHasNonPushOp);
                }

                last_op = Some(another_op);
            }
            Instruction::PushBytes(bytes) => {
                if inside_envelope {
                    
                    // this looks ugly but we need to have least amount of
                    // iterations possible in a malicous case
                    // so if any of the conditions does not hold
                    // we return an error
                    if inside_envelope_index == 0 && bytes.as_bytes() != ROLLUP_NAME_TAG {
                        return Err(ParserError::EnvelopeHasIncorrectFormat);
                    } else if inside_envelope_index == 1 && bytes.as_bytes() != rollup_name.as_bytes() {
                        return Err(ParserError::InvalidRollupName);
                    } else if inside_envelope_index == 2 && bytes.as_bytes() != SIGNATURE_TAG {
                        return Err(ParserError::EnvelopeHasIncorrectFormat);
                    } else if inside_envelope_index == 3 {
                        signature.extend(bytes.as_bytes());
                    }  else if inside_envelope_index == 4 && bytes.as_bytes() != PUBLICKEY_TAG {
                        return Err(ParserError::EnvelopeHasIncorrectFormat);
                    } else if inside_envelope_index == 5 {
                        public_key.extend(bytes.as_bytes());
                    }  else if inside_envelope_index == 6 && bytes.as_bytes() != RANDOM_TAG {
                        return Err(ParserError::EnvelopeHasIncorrectFormat);
                    } else if inside_envelope_index == 8 && bytes.as_bytes() != BODY_TAG {
                        return Err(ParserError::EnvelopeHasIncorrectFormat);
                    } else if inside_envelope_index >= 9 {
                        body.extend(bytes.as_bytes());
                    }

                    inside_envelope_index += 1;
                } else {
                    if bytes.len() == 0 {
                        last_op = Some(OP_FALSE); // rust bitcoin pushes [] instead of op_false
                    }
                }
            }
        }
    }

    if body.len() == 0 || signature.len() == 0 || public_key.len() == 0 {
        return Err(ParserError::EnvelopeHasIncorrectFormat);
    }

    Ok(ParsedInscription {
        body,
        signature,
        public_key,
    })
}

#[cfg(test)]
mod tests {
    use bitcoin::{
        key::XOnlyPublicKey,
        opcodes::{
            all::{OP_CHECKSIG, OP_ENDIF, OP_IF},
            OP_FALSE, OP_TRUE,
        },
        script::{self, PushBytesBuf},
        Transaction,
    };

    use crate::helpers::parsers::{parse_transaction, ParserError};

    use super::{
        parse_relevant_inscriptions, BODY_TAG, PUBLICKEY_TAG, RANDOM_TAG, ROLLUP_NAME_TAG,
        SIGNATURE_TAG,
    };

    #[test]
    fn correct() {
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_ok());

        let result = result.unwrap();

        assert_eq!(result.body, vec![0u8; 128]);
        assert_eq!(result.signature, vec![0u8; 64]);
        assert_eq!(result.public_key, vec![0u8; 64]);
    }

    #[test]
    fn wrong_rollup_tag() {
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("not-sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ParserError::InvalidRollupName);
    }

    #[test]
    fn leave_out_tags() {
        // name
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err(), "Failed to error on no name tag.");
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);

        // signature
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err(), "Failed to error on no signature tag.");
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);

        // publickey
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err(), "Failed to error on no publickey tag.");
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);

        // body
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err(), "Failed to error on no body tag.");

        // random
        let reveal_script_builder = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF);

        let reveal_script = reveal_script_builder.into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err(), "Failed to error on no random tag.");
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);
    }

    #[test]
    fn non_parseable_tx() {
        let hex_tx = "020000000001013a66019bfcc719ba12586a83ebbb0b3debdc945f563cd64fd44c8044e3d3a1790100000000fdffffff028fa2aa060000000017a9147ba15d4e0d8334de3a68cf3687594e2d1ee5b00d879179e0090000000016001493c93ad222e57d65438545e048822ede2d418a3d0247304402202432e6c422b93705fbc57b350ea43e4ef9441c0907988eff051eaac807fc8cf2022046c92b540b5f04f8da11febb5d2a478aed1b8bc088e769da8b78fffcae8c9a9a012103e2991b47d9c788f55379f9ef519b642d79d7dfe0e7555ec5575ee934b2dca1223f5d0c00";

        let tx: Transaction =
            bitcoin::consensus::deserialize(&hex::decode(hex_tx).unwrap()).unwrap();

        let result = parse_transaction(&tx, "sov-btc");

        assert!(result.is_err(), "Failed to error on non-parseable tx.");
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);
    }

    #[test]
    fn only_checksig() {
        let reveal_script = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasIncorrectFormat);
    }

    #[test]
    fn complex_envelope() {
        let reveal_script = script::Builder::new()
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_opcode(OP_TRUE)
            .push_opcode(OP_IF)
            .push_slice(XOnlyPublicKey::from_slice(&[2; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_ENDIF)
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF)
            .into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ParserError::EnvelopeHasNonPushOp);
    }

    #[test]
    fn two_envelopes() {
        let reveal_script = script::Builder::new()
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 128]).unwrap())
            .push_opcode(OP_ENDIF)
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([1u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(1)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 128]).unwrap())
            .push_opcode(OP_ENDIF)
            .into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_ok());

        let result = result.unwrap();

        assert_eq!(result.body, vec![0u8; 128]);
        assert_eq!(result.signature, vec![0u8; 64]);
        assert_eq!(result.public_key, vec![0u8; 64]);
    }

    #[test]  
    fn big_push() {
        let reveal_script = script::Builder::new()
            .push_opcode(OP_FALSE)
            .push_opcode(OP_IF)
            .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from("sov-btc".as_bytes().to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from([0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![0u8; 64]).unwrap())
            .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap())
            .push_int(0)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_slice(PushBytesBuf::try_from(vec![1u8; 512]).unwrap())
            .push_opcode(OP_ENDIF)
            .push_slice(XOnlyPublicKey::from_slice(&[1; 32]).unwrap().serialize())
            .push_opcode(OP_CHECKSIG)
            .into_script();

        let result =
            parse_relevant_inscriptions(&mut reveal_script.instructions().peekable(), "sov-btc");

        assert!(result.is_ok());

        let result = result.unwrap();

        assert_eq!(result.body, vec![1u8; 512 * 6]);
        assert_eq!(result.signature, vec![0u8; 64]);
        assert_eq!(result.public_key, vec![0u8; 64]);
    }

}
