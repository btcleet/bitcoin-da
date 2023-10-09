use core::result::Result::Ok;
use core::str::FromStr;
use std::fs::File;
use std::io::{BufWriter, Write};

use anyhow::anyhow;
use bitcoin::{
    absolute::LockTime,
    blockdata::{
        opcodes::{
            all::{OP_CHECKSIG, OP_ENDIF, OP_IF},
            OP_FALSE,
        },
        script,
    },
    hashes::{sha256d, Hash},
    key::{TapTweak, TweakedPublicKey, UntweakedKeyPair},
    psbt::Prevouts,
    script::PushBytesBuf,
    secp256k1::{
        self, constants::SCHNORR_SIGNATURE_SIZE, schnorr::Signature, Secp256k1, XOnlyPublicKey,
    },
    sighash::SighashCache,
    taproot::{ControlBlock, LeafVersion, TapLeafHash, TaprootBuilder},
    Address, Network, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
};
use brotli::{CompressorWriter, DecompressorWriter};

use crate::helpers::{BODY_TAG, PUBLICKEY_TAG, RANDOM_TAG, ROLLUP_NAME_TAG, SIGNATURE_TAG};
use crate::spec::utxo::UTXO;

pub fn compress_blob(blob: &[u8]) -> Vec<u8> {
    let mut writer = CompressorWriter::new(Vec::new(), 4096, 11, 22);
    writer.write_all(blob).unwrap();
    writer.into_inner()
}

pub fn decompress_blob(blob: &[u8]) -> Vec<u8> {
    let mut writer = DecompressorWriter::new(Vec::new(), 4096);
    writer.write_all(blob).unwrap();
    writer.into_inner().expect("decompression failed")
}

// Signs a message with a private key
pub fn sign_blob_with_private_key(
    blob: &[u8],
    private_key: &str,
) -> Result<(Vec<u8>, Vec<u8>), ()> {
    let message = sha256d::Hash::hash(blob).to_byte_array();
    let secp = Secp256k1::new();
    let key = secp256k1::SecretKey::from_str(private_key).unwrap();
    let public_key = secp256k1::PublicKey::from_secret_key(&secp, &key);
    let msg = secp256k1::Message::from_slice(&message).unwrap();
    let sig = secp.sign_ecdsa(&msg, &key);
    Ok((
        sig.serialize_compact().to_vec(),
        public_key.serialize().to_vec(),
    ))
}

fn get_size(
    inputs: &Vec<TxIn>,
    outputs: &Vec<TxOut>,
    script: Option<&ScriptBuf>,
    control_block: Option<&ControlBlock>,
) -> usize {
    let mut tx = Transaction {
        input: inputs.clone(),
        output: outputs.clone(),
        lock_time: LockTime::ZERO,
        version: 1,
    };

    tx.input[0].witness.push(
        Signature::from_slice(&[0; SCHNORR_SIGNATURE_SIZE])
            .unwrap()
            .as_ref(),
    );

    if script.is_some() && control_block.is_some() {
        tx.input[0].witness.push(script.unwrap());
        tx.input[0].witness.push(control_block.unwrap().serialize());
    }

    tx.vsize()
}

fn choose_utxos(utxos: &Vec<UTXO>, amount: u64) -> Result<(Vec<UTXO>, u64), anyhow::Error> {
    let mut bigger_utxos: Vec<&UTXO> = utxos.iter().filter(|utxo| utxo.amount >= amount).collect();
    let mut sum: u64 = 0;
    if bigger_utxos.len() > 0 {
        // sort vec by amount (small first)
        bigger_utxos.sort_by(|a, b| a.amount.cmp(&b.amount));

        // single utxo will be enough
        // so return the transaction
        let utxo = bigger_utxos[0];
        sum += utxo.amount;

        return Ok((vec![utxo.clone()], sum));
    } else {
        let mut smaller_utxos: Vec<&UTXO> =
            utxos.iter().filter(|utxo| utxo.amount < amount).collect();

        // sort vec by amount (large first)
        smaller_utxos.sort_by(|a, b| b.amount.cmp(&a.amount));

        let mut chosen_utxos: Vec<UTXO> = vec![];

        for utxo in smaller_utxos {
            sum += utxo.amount;
            chosen_utxos.push(utxo.clone());

            if sum >= amount {
                break;
            }
        }

        if sum < amount {
            return Err(anyhow!("not enought UTXOs"));
        }

        Ok((chosen_utxos, sum))
    }
}

fn build_commit_transaction(
    utxos: Vec<UTXO>,
    recipient: Address,
    output_value: u64,
    fee_rate: f64,
) -> Result<Transaction, anyhow::Error> {
    // get single input single output transaction size
    let mut size = get_size(
        &vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
                vout: 0,
            },
            script_sig: script::Builder::new().into_script(),
            witness: Witness::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        }],
        &vec![TxOut {
            script_pubkey: recipient.clone().script_pubkey(),
            value: output_value,
        }],
        None,
        None,
    );
    let mut last_size = size;

    let utxos = utxos
        .iter()
        .filter(|utxo| utxo.spendable && utxo.solvable && utxo.amount > 546)
        .map(|u| u.clone())
        .collect::<Vec<UTXO>>();

    if utxos.len() == 0 {
        return Err(anyhow::anyhow!("no spendable utxos"));
    }

    let tx = loop {
        let fee = ((size as f64) * fee_rate).ceil() as u64;

        let input_total = output_value + fee;

        let res = choose_utxos(&utxos, input_total);

        if res.is_err() {
            return Err(anyhow::anyhow!("utxos are not enough"));
        }

        let (chosen_utxos, sum) = res.unwrap();

        let mut outputs: Vec<TxOut> = vec![];

        outputs.push(TxOut {
            value: output_value,
            script_pubkey: recipient.script_pubkey(),
        });

        let excess = sum.checked_sub(input_total);

        if excess.is_some() && excess.unwrap() >= 546 {
            outputs.push(TxOut {
                value: sum - input_total,
                script_pubkey: recipient.script_pubkey(),
            });
        }

        let inputs = chosen_utxos
            .iter()
            .map(|u| TxIn {
                previous_output: OutPoint {
                    txid: u.tx_id,
                    vout: u.vout,
                },
                script_sig: script::Builder::new().into_script(),
                witness: Witness::new(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
            })
            .collect();

        size = get_size(&inputs, &outputs, None, None);

        if size == last_size {
            break Transaction {
                lock_time: LockTime::ZERO,
                version: 1,
                input: inputs,
                output: outputs,
            };
        }

        last_size = size;
    };

    Ok(tx)
}

fn build_reveal_transaction(
    input_utxo: TxOut,
    input_txid: Txid,
    input_vout: u32,
    recipient: Address,
    output_value: u64,
    fee_rate: f64,
    reveal_script: &ScriptBuf,
    control_block: &ControlBlock,
) -> Result<Transaction, anyhow::Error> {
    let mut size = get_size(
        &vec![TxIn {
            previous_output: OutPoint {
                txid: Txid::from_str(
                    "0000000000000000000000000000000000000000000000000000000000000000",
                )
                .unwrap(),
                vout: 0,
            },
            script_sig: script::Builder::new().into_script(),
            witness: Witness::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        }],
        &vec![TxOut {
            script_pubkey: recipient.clone().script_pubkey(),
            value: output_value,
        }],
        Some(reveal_script),
        Some(control_block),
    );
    let mut last_size = size;

    if input_utxo.value < 546 {
        return Err(anyhow::anyhow!("input utxo not big enough"));
    }

    let tx = loop {
        let fee = ((size as f64) * fee_rate).ceil() as u64;

        let input_total = output_value + fee;

        let mut outputs: Vec<TxOut> = vec![];

        outputs.push(TxOut {
            value: output_value,
            script_pubkey: recipient.script_pubkey(),
        });

        let excess = input_utxo.value.checked_sub(input_total);

        if excess.is_some() && excess.unwrap() >= 546 {
            outputs.push(TxOut {
                value: input_utxo.value - input_total,
                script_pubkey: recipient.script_pubkey(),
            });
        }

        let inputs = vec![TxIn {
            previous_output: OutPoint {
                txid: input_txid,
                vout: input_vout,
            },
            script_sig: script::Builder::new().into_script(),
            witness: Witness::new(),
            sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
        }];

        size = get_size(&inputs, &outputs, Some(reveal_script), Some(control_block));

        if size == last_size {
            break Transaction {
                lock_time: LockTime::ZERO,
                version: 1,
                input: inputs,
                output: outputs,
            };
        }

        last_size = size;
    };

    Ok(tx)
}

// TODO: parametrize hardness
// so tests are easier
// Creates the inscription transactions (commit and reveal)
pub fn create_inscription_transactions(
    rollup_name: &str,
    body: Vec<u8>,
    signature: Vec<u8>,
    sequencer_public_key: Vec<u8>,
    utxos: Vec<UTXO>,
    recipient: Address,
    commit_fee_rate: f64,
    reveal_fee_rate: f64,
    network: Network,
) -> Result<(Transaction, Transaction), anyhow::Error> {
    // Create commit key
    let secp256k1 = Secp256k1::new();
    let key_pair = UntweakedKeyPair::new(&secp256k1, &mut rand::thread_rng());
    let (public_key, _parity) = XOnlyPublicKey::from_keypair(&key_pair);

    // start creating inscription content
    let reveal_script_builder = script::Builder::new()
        .push_x_only_key(&public_key)
        .push_opcode(OP_CHECKSIG)
        .push_opcode(OP_FALSE)
        .push_opcode(OP_IF)
        .push_slice(PushBytesBuf::try_from(ROLLUP_NAME_TAG.to_vec()).unwrap())
        .push_slice(PushBytesBuf::try_from(rollup_name.as_bytes().to_vec()).unwrap())
        .push_slice(PushBytesBuf::try_from(SIGNATURE_TAG.to_vec()).unwrap())
        .push_slice(PushBytesBuf::try_from(signature).unwrap())
        .push_slice(PushBytesBuf::try_from(PUBLICKEY_TAG.to_vec()).unwrap())
        .push_slice(PushBytesBuf::try_from(sequencer_public_key).unwrap())
        .push_slice(PushBytesBuf::try_from(RANDOM_TAG.to_vec()).unwrap());
    // This envelope is not finished yet. The random number will be added later and followed by the body

    // Start loop to find a random number that makes the first two bytes of the reveal tx hash 0
    let mut random: i64 = 0;
    loop {
        let utxos = utxos.clone();
        let recipient = recipient.clone();
        // ownerships are moved to the loop
        let mut reveal_script_builder = reveal_script_builder.clone();

        // push first random number and body tag
        reveal_script_builder = reveal_script_builder
            .push_int(random)
            .push_slice(PushBytesBuf::try_from(BODY_TAG.to_vec()).unwrap());

        // push body in chunks of 520 bytes
        for chunk in body.chunks(520) {
            reveal_script_builder =
                reveal_script_builder.push_slice(PushBytesBuf::try_from(chunk.to_vec()).unwrap());
        }
        // push end if
        reveal_script_builder = reveal_script_builder.push_opcode(OP_ENDIF);

        // finalize reveal script
        let reveal_script = reveal_script_builder.into_script();

        // create spend info for tapscript
        let taproot_spend_info = TaprootBuilder::new()
            .add_leaf(0, reveal_script.clone())
            .unwrap()
            .finalize(&secp256k1, public_key)
            .unwrap();

        // create control block for tapscript
        let control_block = taproot_spend_info
            .control_block(&(reveal_script.clone(), LeafVersion::TapScript))
            .unwrap();

        // create commit tx address
        let commit_tx_address = Address::p2tr(
            &secp256k1,
            public_key,
            taproot_spend_info.merkle_root(),
            network,
        );

        // build commit tx
        let unsigned_commit_tx =
            build_commit_transaction(utxos, commit_tx_address.clone(), 546, commit_fee_rate)?;

        let output_to_reveal = unsigned_commit_tx.output[0].clone();

        let mut reveal_tx = build_reveal_transaction(
            output_to_reveal.clone(),
            unsigned_commit_tx.txid(),
            0,
            recipient,
            546,
            reveal_fee_rate,
            &reveal_script,
            &control_block,
        )?;

        let reveal_hash = reveal_tx.txid().as_raw_hash().to_byte_array();

        // check if first two bytes are 0
        if reveal_hash.starts_with(&[0, 0]) {
            // start signing reveal tx
            let mut sighash_cache = SighashCache::new(&mut reveal_tx);

            // create data to sign
            let signature_hash = sighash_cache
                .taproot_script_spend_signature_hash(
                    0,
                    &Prevouts::All(&[output_to_reveal]),
                    TapLeafHash::from_script(&reveal_script, LeafVersion::TapScript),
                    bitcoin::sighash::TapSighashType::Default,
                )
                .unwrap();

            // sign reveal tx data
            let signature = secp256k1.sign_schnorr(
                &secp256k1::Message::from_slice(signature_hash.as_byte_array())
                    .expect("should be cryptographically secure hash"),
                &key_pair,
            );

            // add signature to witness and finalize reveal tx
            let witness = sighash_cache.witness_mut(0).unwrap();
            witness.push(signature.as_ref());
            witness.push(reveal_script);
            witness.push(&control_block.serialize());

            // check if inscription locked to the correct address
            let recovery_key_pair =
                key_pair.tap_tweak(&secp256k1, taproot_spend_info.merkle_root());
            let (x_only_pub_key, _parity) = recovery_key_pair.to_inner().x_only_public_key();
            assert_eq!(
                Address::p2tr_tweaked(
                    TweakedPublicKey::dangerous_assume_tweaked(x_only_pub_key),
                    network,
                ),
                commit_tx_address
            );

            return Ok((unsigned_commit_tx, reveal_tx));
        }

        random += 1;
    }
}

pub fn write_reveal_tx(tx: &[u8], tx_id: String) {
    let reveal_tx_file = File::create(format!("reveal_{}.tx", tx_id)).unwrap();
    let mut reveal_tx_writer = BufWriter::new(reveal_tx_file);
    reveal_tx_writer.write_all(tx).unwrap();
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;

    use bitcoin::{hashes::Hash, Address, Txid};

    use crate::{
        helpers::{
            builders::{compress_blob, decompress_blob},
            parsers::parse_transaction,
        },
        spec::utxo::UTXO,
    };

    #[test]
    fn compression_decompression() {
        let blob = std::fs::read("test_data/blob.txt").unwrap();

        // compress and measure time
        let time = std::time::Instant::now();
        let compressed_blob = compress_blob(&blob);
        println!("compression time: {:?}", time.elapsed());

        // decompress and measure time
        let time = std::time::Instant::now();
        let decompressed_blob = decompress_blob(&compressed_blob);
        println!("decompression time: {:?}", time.elapsed());

        assert_eq!(blob, decompressed_blob);

        // size
        println!("blob size: {}", blob.len());
        println!("compressed blob size: {}", compressed_blob.len());
        println!(
            "compression ratio: {}",
            (blob.len() as f64) / (compressed_blob.len() as f64)
        );
    }

    #[test]
    fn write_reveal_tx() {
        let tx = vec![100, 100, 100];
        let tx_id = "test_tx".to_string();

        super::write_reveal_tx(tx.as_slice(), tx_id);

        let file = std::fs::read("reveal_test_tx.tx").unwrap();

        assert_eq!(tx, file);

        std::fs::remove_file("reveal_test_tx.tx").unwrap();
    }

    fn get_mock_data() -> (&'static str, Vec<u8>, Vec<u8>, Vec<u8>, Address, Vec<UTXO>) {
        let rollup_name = "test_rollup";
        let body = vec![100; 1000];
        let signature = vec![100; 64];
        let sequencer_public_key = vec![100; 33];
        let address = Address::from_str("bc1qf6cfk4nd875y9tyey7eyetwnlsx6t3yvdtd0wl")
            .unwrap()
            .require_network(bitcoin::Network::Bitcoin)
            .unwrap();
        let utxos = vec![
            UTXO {
                tx_id: Txid::from_str(
                    "4cfbec13cf1510545f285cceceb6229bd7b6a918a8f6eba1dbee64d26226a3b7",
                )
                .unwrap(),
                vout: 0,
                address: "bc1qf6cfk4nd875y9tyey7eyetwnlsx6t3yvdtd0wl".to_string(),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 1_000_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
            UTXO {
                tx_id: Txid::from_str(
                    "44990141674ff56ed6fee38879e497b2a726cddefd5e4d9b7bf1c4e561de4347",
                )
                .unwrap(),
                vout: 0,
                address: "bc1qf6cfk4nd875y9tyey7eyetwnlsx6t3yvdtd0wl".to_string(),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 100_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
            UTXO {
                tx_id: Txid::from_str(
                    "4dbe3c10ee0d6bf16f9417c68b81e963b5bccef3924bbcb0885c9ea841912325",
                )
                .unwrap(),
                vout: 0,
                address: "bc1qf6cfk4nd875y9tyey7eyetwnlsx6t3yvdtd0wl".to_string(),
                script_pubkey: address.script_pubkey().to_hex_string(),
                amount: 10_000,
                confirmations: 100,
                spendable: true,
                solvable: true,
            },
        ];

        return (
            rollup_name,
            body,
            signature,
            sequencer_public_key,
            address,
            utxos,
        );
    }

    #[test]
    fn choose_utxos() {
        let (_, _, _, _, _, utxos) = get_mock_data();

        let (chosen_utxos, sum) = super::choose_utxos(&utxos, 105_000).unwrap();

        assert_eq!(sum, 1_000_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[0]);

        let (chosen_utxos, sum) = super::choose_utxos(&utxos, 1_005_000).unwrap();

        assert_eq!(sum, 1_100_000);
        assert_eq!(chosen_utxos.len(), 2);
        assert_eq!(chosen_utxos[0], utxos[0]);
        assert_eq!(chosen_utxos[1], utxos[1]);

        let (chosen_utxos, sum) = super::choose_utxos(&utxos, 100_000).unwrap();

        assert_eq!(sum, 100_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[1]);

        let (chosen_utxos, sum) = super::choose_utxos(&utxos, 90_000).unwrap();

        assert_eq!(sum, 100_000);
        assert_eq!(chosen_utxos.len(), 1);
        assert_eq!(chosen_utxos[0], utxos[1]);

        let res = super::choose_utxos(&utxos, 100_000_000);

        assert!(res.is_err());
        assert_eq!(format!("{}", res.unwrap_err()), "not enought UTXOs");
    }

    #[test]
    fn create_inscription_transactions() {
        let (rollup_name, body, signature, sequencer_public_key, address, utxos) = get_mock_data();

        let (commit, reveal) = super::create_inscription_transactions(
            rollup_name,
            body.clone(),
            signature.clone(),
            sequencer_public_key.clone(),
            utxos.clone(),
            address.clone(),
            12.0,
            10.0,
            bitcoin::Network::Bitcoin,
        )
        .unwrap();

        // check pow
        assert!(reveal.txid().as_byte_array().starts_with(&[0, 0]));

        // check outputs
        assert_eq!(commit.output.len(), 2, "commit tx should have 2 outputs");

        assert_eq!(reveal.output.len(), 1, "reveal tx should have 1 output");

        assert_eq!(
            commit.input[0].previous_output.txid, utxos[2].tx_id,
            "utxo to inscribe should be chosen correctly"
        );
        assert_eq!(
            commit.input[0].previous_output.vout, utxos[2].vout,
            "utxo to inscribe should be chosen correctly"
        );

        assert_eq!(
            reveal.input[0].previous_output.txid,
            commit.txid(),
            "reveal should use commit as input"
        );
        assert_eq!(
            reveal.input[0].previous_output.vout, 0,
            "reveal should use commit as input"
        );

        assert_eq!(
            reveal.output[0].script_pubkey,
            address.script_pubkey(),
            "reveal should pay to the correct address"
        );

        // check inscription
        let inscription = parse_transaction(&reveal, rollup_name).unwrap();

        assert_eq!(inscription.body, body, "body should be correct");
        assert_eq!(
            inscription.signature, signature,
            "signature should be correct"
        );
        assert_eq!(
            inscription.public_key, sequencer_public_key,
            "sequencer public key should be correct"
        );
    }
}
