use anyhow::{Result, Context, bail};

use rand::{RngCore, SeedableRng};
use rand_chacha::ChaCha20Rng;
use blst::min_pk::{SecretKey, PublicKey, Signature, AggregatePublicKey, AggregateSignature};
use blst::BLST_ERROR;
use ecies::{decrypt, encrypt, utils::generate_keypair};
use ecies::PublicKey as EthPublicKey;
use ecies::SecretKey as EthSecretKey;
use sha3::{Digest, Keccak256};

use std::path::PathBuf;
use std::fs;

pub const CIPHER_SUITE: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_NUL_";

/// Generates Eth secret and public key, then saves the key using the
/// ETH address derived from the public key as the filename.
pub fn eth_key_gen() -> Result<EthPublicKey> {
    let (sk, pk) = new_eth_key()?;
    save_eth_key(sk, pk).with_context(|| "Failed to save generated ETH key")
}

/// Wrapper around ecies utility function to generate SECP256K1 keypair
pub fn new_eth_key() -> Result<(EthSecretKey, EthPublicKey)> {
    Ok(generate_keypair())
}

/// keccak hash function to hash arbitrary bytes to 32 bytes 
pub fn keccak(bytes: &[u8]) -> Result<[u8; 32]> {
    // create a Keccak256 object
    let mut hasher = Keccak256::new();

    // write input message
    hasher.update(bytes);

    // read hash digest
    let digest: [u8; 32] = hasher.finalize()
        .as_slice()
        .try_into()
        .with_context(|| "keccak could not be cast to [u8; 32,]")?;

    Ok(digest)
}

/// Converts an Eth pbulic key to a wallet address, encoded as hex string
pub fn pk_to_eth_addr(pk: &EthPublicKey) -> Result<String> {
    // get the uncompressed PK in bytes (should be 65)
    let pk_bytes = pk.serialize();

    if pk_bytes.len() != 65 {
        bail!("SECP256K1 pub key must be 65B, len was: {}", pk_bytes.len());
    }

    // hash of PK bytes (skip the 1st byte)
    let digest: [u8; 32] = keccak(&pk_bytes[1..]).with_context(|| "keccak failed when converting pk to eth addr")?;

    // keep the last 20 bytes
    let last = &digest[12..];

    // encode the bytes as a hex string
    let hex_str = hex::encode(last);

    // Convert to eth checksum address
    checksum(hex_str.as_str())
}

// Adapted from: https://github.com/miguelmota/rust-eth-checksum/
pub fn checksum(address: &str) -> Result<String> {
    let address = address.trim_start_matches("0x").to_lowercase();

    let hash_bytes = keccak(address.as_bytes())?;
    let address_hash_string = hex::encode(&hash_bytes);
    let address_hash = address_hash_string.as_str();


    Ok(address
        .char_indices()
        .fold(String::from("0x"), |mut acc, (index, address_char)| {
            // this cannot fail since it's Keccak256 hashed
            let n = u16::from_str_radix(&address_hash[index..index + 1], 16).unwrap();

            if n > 7 {
                // make char uppercase if ith character is 9..f
                acc.push_str(&address_char.to_uppercase().to_string())
            } else {
                // already lowercased
                acc.push(address_char)
            }

            acc
        }))
}

/// write the Eth SECP256K1 secret key to a secure file using the derived 
/// Eth wallet address as the file name
fn save_eth_key(sk: EthSecretKey, pk: EthPublicKey) -> Result<EthPublicKey> {
    // convert the pk to an eth address
    let addr = pk_to_eth_addr(&pk).with_context(|| "couldnt convert pk to eth addr to use as file name")?;
    println!("new enclave address: {}", addr);

    let sk_hex = hex::encode(sk.serialize());
    println!("debug sk_hex: {:?}", sk_hex);

    write_key(&format!("eth_keys/{}", addr), &sk_hex).with_context(|| "eth sk failed to save")?;

    Ok(pk)
}

/// Generates a new BLS secret key from randomness
pub fn new_bls_key() -> Result<SecretKey> {
    // rng
    let mut rng = ChaCha20Rng::from_entropy();
    let mut ikm = [0u8; 32];
    rng.fill_bytes(&mut ikm);

    // key gen
    let sk = SecretKey::key_gen(&ikm, &[]);

    match sk.as_ref().err() {
        Some(BLST_ERROR::BLST_SUCCESS) | None => Ok(sk.unwrap()),
        Some(_) => bail!("Failed to generate BLS sk"),
    }
}

/// Generates and saves BLS secret key, using derived pk_hex as file identifier (omitting '0x' prefix)
pub fn bls_key_gen(save_key: bool) -> Result<PublicKey> {
    let sk = new_bls_key()?;
    let pk = sk.sk_to_pk();
    let pk_bytes: [u8; 48] = sk.sk_to_pk().compress();

    // compress pk to 48B
    let pk_hex: String = hex::encode(pk_bytes);
    let sk_hex: String = hex::encode(sk.to_bytes());

    // save secret key using pk_hex as file identifier (omitting '0x' prefix)
    if save_key {
        write_key(&format!("bls_keys/{}", pk_hex), &sk_hex).with_context(|| "failed to save bls key")?;
    }

    Ok(pk)
}

/// Generates a BLS secret key then encrypts via ECDH using pk_hex
// pub fn bls_key_provision(eth_pk_hex: &String) -> Result<(Vec<u8>, PublicKey)> {
pub fn bls_key_provision(eth_pk_hex: &String) -> Result<(String, PublicKey)> {
    let sk = new_bls_key()?;
    let pk = sk.sk_to_pk();
    let receiver_pub = hex::decode(eth_pk_hex)
        .with_context(|| format!("couldnt decode eth_pk_hex in bls_key_provision: {}", eth_pk_hex))?;

    let ct_sk_bytes = encrypt(&receiver_pub, &sk.serialize())
        .with_context(|| format!("Couldn't encrypt bls sk with pk {}", eth_pk_hex))?;

    let ct_sk_hex = hex::encode(ct_sk_bytes);

    Ok((ct_sk_hex, pk))
}

/// Generates `n` BLS secret keys and derives one `n/n` aggregate public key from it
pub fn dist_bls_key_gen(n: usize) -> Result<(AggregatePublicKey, Vec<SecretKey>)> {
    // generate n sks
    let mut sks: Vec<SecretKey> = Vec::new();
    for i in 0..n {
        match new_bls_key(){
            Ok(sk) => sks.push(sk),
            Err(e) => bail!("Failed to generate BLS sk {} in dist_bls_key_gen(), blst error: {}", i, e),
        }
    }

    // derive n pks
    let pks: Vec<PublicKey> = sks.iter().map(|sk| {
        let pk = sk.sk_to_pk();
        println!("pk: {:?}", hex::encode(pk.to_bytes()));
        pk
    }).collect();
    let pks_refs: Vec<&PublicKey> = pks.iter().map(|pk| pk).collect();

    // aggregate the n BLS public keys into 1 aggregate pk
    let agg_pk_res = AggregatePublicKey::aggregate(&pks_refs, true);
    match agg_pk_res.err() {
        Some(BLST_ERROR::BLST_SUCCESS) | None => {
            let agg_pk = agg_pk_res.unwrap();
            println!("agg_pk: {:?}", hex::encode(agg_pk.to_public_key().to_bytes()));
            Ok((agg_pk, sks))
        },
        _ => bail!("Failed to aggregate BLS pub keys"),
    }
}

/// Returns Ok() if `sig` is a valid BLS signature 
pub fn verify_bls_sig(sig: Signature, pk: PublicKey, msg: &[u8]) -> Result<()> {
    match sig.verify(
        true, // sig_groupcheck
        msg, // msg
        CIPHER_SUITE, // dst
        &[], // aug
        &pk, // pk
        true) { // pk_validate 
            BLST_ERROR::BLST_SUCCESS => Ok(()),
            _ => bail!("BLS Signature verifcation failed")
        }
}

/// Performs BLS signnature on `msg` using the BLS secret key looked up from memory
/// with pk_hex as the file identifier. 
fn bls_sign(pk_hex: &String, msg: &[u8]) -> Result<Signature> {
    // read pk
    let pk_bytes = hex::decode(pk_hex)?;
    let pk_res = PublicKey::from_bytes(&pk_bytes);

    let pk = match pk_res.as_ref().err() {
        Some(BLST_ERROR::BLST_SUCCESS) | None => {
            let pk = pk_res.unwrap();
            println!("pk: {:?}", hex::encode(pk.to_bytes()));
            pk
        },
        _ => bail!("Could not recover pk from pk_hex"),
    };

    // read sk
    let sk = read_bls_key(&pk_hex)?;

    // valid keypair
    if sk.sk_to_pk() != pk {
        bail!("Mismatch with input and derived pk");
    }
    println!("DEBUG: sk recovered {:?}", sk);

    // sign the message
    let sig = sk.sign(msg, CIPHER_SUITE, &[]);

    // verify the signatures correctness
    verify_bls_sig(sig, pk, msg)?;

    // Return the BLS signature
    Ok(sig)
}

/// Writes the hex-encoded secret key to a file named from `fname`
pub fn write_key(fname: &String, sk_hex: &String) -> Result<()> {
    let file_path: PathBuf = ["./etc/keys/", fname.as_str()].iter().collect();
    if let Some(p) = file_path.parent() { 
        fs::create_dir_all(p).with_context(|| "Failed to create keys dir")?
    }; 
    fs::write(&file_path, sk_hex).with_context(|| "failed to write sk")
}

/// Reads hex-encoded secret key from a file named from `pk_hex` and converts it to a BLS SecretKey
pub fn read_bls_key(pk_hex: &String) -> Result<SecretKey> {
    let file_path: PathBuf = ["./etc/keys/", pk_hex.as_str()].iter().collect();
    let sk_rec_bytes = fs::read(&file_path).with_context(|| format!("Unable to read bls sk from pk_hex {}", pk_hex))?;
    let sk_rec_dec = hex::decode(sk_rec_bytes).with_context(|| "Unable to decode sk hex")?;
    let sk_res = SecretKey::from_bytes(&sk_rec_dec);

    match sk_res.as_ref().err() { 
        Some(BLST_ERROR::BLST_SUCCESS) | None => {
            Ok(sk_res.unwrap())
        },
        _ => bail!("Could not read_bls_key from pk_hex {}", pk_hex),
    }
}

/// Reads hex-encoded secret key from a file named from `pk_hex` and converts it to an Eth SecretKey
pub fn read_eth_key(fname: &String) -> Result<EthSecretKey> {
    let file_path: PathBuf = ["./etc/keys/", fname.as_str()].iter().collect();
    let sk_rec_bytes = fs::read(&file_path).with_context(|| "Unable to read eth secret key")?;
    let sk_rec_dec = hex::decode(sk_rec_bytes).with_context(|| "Unable to decode sk hex")?;
    EthSecretKey::parse_slice(&sk_rec_dec).with_context(|| "couldn't parse sk bytes to eth sk type")
}

/// Returns the file names of each of the saved secret keys, where each fname
/// is assumed to be the compressed public key in hex without the `0x` prefix.
pub fn list_bls_keys() -> Result<Vec<String>> {
    let paths = fs::read_dir("./etc/keys/").with_context(|| "No keys saved in dir")?;

    let mut keys: Vec<String> = Vec::new();
    for path in paths {
        // Get the paths to each file in this dir
        let p = match path.as_ref().err() {
            Some(e) => bail!("failed to find path: {}", e),
            _ => path.unwrap(),
        };

        // remove path prefix, to grab just the file name
        let fname = p.file_name();

        match fname.to_os_string().into_string() {
            Ok(s) => keys.push(s),
            Err(e) => bail!("Error, bad file name in list_keys(): {:?}", e),
        }
    }
    Ok(keys)
}

pub fn aggregate_uniform_bls_sigs(agg_pk: AggregatePublicKey, sigs: Vec<&Signature>, 
    msg: &[u8]) -> Result<()> {
    let n = sigs.len();
    assert!(n > 0);

    // aggregate the n signatures into 1 
    let agg = match AggregateSignature::aggregate(&sigs, true) {
        Ok(agg) => agg,
        Err(err) => bail!("could not aggregate the signatures {:?}", err),
    };
    let agg_sig = agg.to_signature();
    println!("agg_sig: {:?}", hex::encode(agg_sig.to_bytes()));

    // verify the aggregate signature using the aggregate pk
    // (ASSUMES msgs are identical)
    match agg_sig.verify(false, msg, CIPHER_SUITE, &[], &agg_pk.to_public_key(), false) {
        BLST_ERROR::BLST_SUCCESS => Ok(()),
        _ => bail!("BLS verify() with aggregate pub key failed to verify aggregate bls signature")
    }
}

pub fn aggregate_non_uniform_bls_sigs(sigs: Vec<&Signature>, pks: Vec<&PublicKey>, 
    msgs: Vec<&[u8]>) -> Result<()> {
    let n = sigs.len();
    assert!(n > 0);
    assert_eq!(n, pks.len());
    assert_eq!(n, msgs.len());

    // verify each signature against the public key in order
    let errs = sigs
        .iter()
        .zip(msgs.iter())
        .zip(pks.iter())
        .map(|((s, m), pk)| {
            s.verify(
                true,
                m, 
                CIPHER_SUITE, 
                &[], 
                pk, 
                true)
        })
        .collect::<Vec<BLST_ERROR>>();

    // check any errors
    if errs != vec![BLST_ERROR::BLST_SUCCESS; n] {
        bail!("There was an invalid signature in the group")
    }

    // aggregate the n signatures into 1 
    let agg = match AggregateSignature::aggregate(&sigs, true) {
        Ok(agg) => agg,
        Err(err) => bail!("could not aggregate the signatures {:?}", err),
    };
    let agg_sig = agg.to_signature();
    println!("agg_sig: {:?}", hex::encode(agg_sig.to_bytes()));

    // verify the aggregate sig using aggregate_verify
    match agg_sig.aggregate_verify(false, &msgs, CIPHER_SUITE, &pks, false) {
        BLST_ERROR::BLST_SUCCESS => Ok(()),
        _ => bail!("BLS aggregate_verify failed to verify signatures")
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use ecies::PublicKey as EthPublicKey;
    use ecies::SecretKey as EthSecretKey;

    #[test]
    fn bls_key_gen_produces_valid_keys_and_sig() -> Result<()> {
        let pk: PublicKey = bls_key_gen(true)?;
        let pk_hex = hex::encode(pk.compress());
        let msg = b"yadayada";
        let sig = bls_sign(&pk_hex, msg)?;
        verify_bls_sig(sig, pk, msg)?;
        Ok(())
    }

    #[test]
    fn eth_key_gen_encryption_works() -> Result<()> {
        let (sk, pk) = new_eth_key()?;
        let msg = b"yadayada";

        // encrypt msg
        let ct = match encrypt(&pk.serialize(), msg) {
            Ok(ct) => ct,
            Err(_) => panic!("Couldn't encrypt msg")
        };

        // decrpyt msg
        let data = match decrypt(&sk.serialize(), &ct) {
            Ok(pt) => pt,
            Err(_) => panic!("Couldn't decrypt msg")
        };

        assert_eq!(msg.to_vec(), data);

        Ok(())
    }

    #[test]
    fn eth_key_gen_key_management() -> Result<()> {
        // gen key and save it to file
        let pk = eth_key_gen()?;
        // rederive eth wallet address filename
        let addr = pk_to_eth_addr(&pk)?;
        // read sk from file
        let sk = read_eth_key(&addr)?;

        let msg = b"yadayada";

        // encrypt msg
        let ct = match encrypt(&pk.serialize(), msg) {
            Ok(ct) => ct,
            Err(_) => panic!("Couldn't encrypt msg")
        };

        // decrpyt msg
        let data = match decrypt(&sk.serialize(), &ct) {
            Ok(pt) => pt,
            Err(_) => panic!("Couldn't decrypt msg")
        };

        assert_eq!(msg.to_vec(), data);

        Ok(())
    }

    #[test]
    fn test_bls_key_provision() -> Result<()> {
        // new eth key pair (assumed the requester knows sk)
        let (sk, pk) = new_eth_key()?;

        let pk_hex = hex::encode(pk.serialize());

        // provision a bls key that is encrypted using ecies and bls_pk
        let (ct_bls_sk_hex, bls_pk) = bls_key_provision(&pk_hex)?;

        // hex decode
        let ct_bls_sk = hex::decode(ct_bls_sk_hex)?;

        // requester can decrypt ct_bls_sk
        let bls_sk_bytes = decrypt(&sk.serialize(), &ct_bls_sk)?;

        // the BLS sk can be recovered from bytes
        let bls_sk = SecretKey::from_bytes(&bls_sk_bytes).unwrap();

        // assert this recovered bls sk derives the expected bls pk
        assert_eq!(bls_sk.sk_to_pk(), bls_pk);
        
        Ok(())
    }

    #[test]
    fn test_aggregate_uniform_msgs() -> Result<()> {
        // number of nodes
        const n: usize = 10;

        let mut rng = ChaCha20Rng::from_entropy();
        let mut msg =[0u8; 256 as usize];
        rng.fill_bytes(&mut msg);
        println!("msg: {:?}", msg);

        let (agg_pk, sks) = dist_bls_key_gen(n)?;

        // derive n pks
        let pks: Vec<PublicKey> = sks.iter().map(|sk| {
            let pk = sk.sk_to_pk();
            // println!("pk: {:?}", hex::encode(pk.to_bytes()));
            pk
        }).collect();

        // each node signs identical msg
        let sigs: Vec<Signature> = sks
            .iter()
            .map(|sk| {
            let sig = sk.sign(&msg, CIPHER_SUITE, &[]);
            println!("sig: {:?}", hex::encode(sig.to_bytes()));
            sig
        }).collect();
        let sigs_refs = sigs.iter().map(|s| s).collect::<Vec<&Signature>>();

        aggregate_uniform_bls_sigs(agg_pk, sigs_refs, &msg)
    }

    #[test]
    #[should_panic]
    fn test_aggregate_uniform_msgs_fails_if_less_than_n_sigs() {
        // number of nodes
        const n: usize = 10;

        let mut rng = ChaCha20Rng::from_entropy();
        let mut msg =[0u8; 256 as usize];
        rng.fill_bytes(&mut msg);
        println!("msg: {:?}", msg);

        let (agg_pk, sks) = dist_bls_key_gen(n).unwrap();

        // derive n pks
        let pks: Vec<PublicKey> = sks.iter().map(|sk| {
            let pk = sk.sk_to_pk();
            // println!("pk: {:?}", hex::encode(pk.to_bytes()));
            pk
        }).collect();

        // each node signs identical msg
        let sigs: Vec<Signature> = sks
            .iter()
            .map(|sk| {
            let sig = sk.sign(&msg, CIPHER_SUITE, &[]);
            println!("sig: {:?}", hex::encode(sig.to_bytes()));
            sig
        }).collect();
        let mut sigs_refs = sigs.iter().map(|s| s).collect::<Vec<&Signature>>();

        // Drop the last signature to force a failure
        sigs_refs.truncate(n - 1);
        assert_eq!(sigs_refs.len(), n - 1);

        aggregate_uniform_bls_sigs(agg_pk, sigs_refs, &msg).unwrap();
    }

    #[test]
    fn test_aggregate_non_uniform_bls_sigs() -> Result<()>{
        // number of nodes
        const n: usize = 10;

        let mut rng = ChaCha20Rng::from_entropy();
        let mut msgs: Vec<Vec<u8>> = vec![vec![]; n];
        for i in 0..n {
            let msg_len = (rng.next_u64() & 0x3F) + 1;
            msgs[i] = vec![0u8; msg_len as usize];
            rng.fill_bytes(&mut msgs[i]);
            println!("msg: {:?}", msgs[i]);
        }

        let msgs_refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();

        let (agg_pk, sks) = dist_bls_key_gen(n)?;

        // derive n pks
        let pks: Vec<PublicKey> = sks.iter().map(|sk| {
            let pk = sk.sk_to_pk();
            // println!("pk: {:?}", hex::encode(pk.to_bytes()));
            pk
        }).collect();
        let pks_refs: Vec<&PublicKey> = pks.iter().map(|pk| pk) .collect();

        // each node signs different msg
        let sigs: Vec<Signature> = sks
            .iter()
            .zip(msgs.clone().into_iter())
            .map(|(sk, msg)| {
            let sig = sk.sign(msg.as_slice(), CIPHER_SUITE, &[]);
            println!("sig: {:?}", hex::encode(sig.to_bytes()));
            sig
        }).collect();
        let sigs_refs = sigs.iter().map(|s| s).collect::<Vec<&Signature>>();

        aggregate_non_uniform_bls_sigs(sigs_refs, pks_refs, msgs_refs)
    }

    #[test]
    #[should_panic]
    fn test_aggregate_non_uniform_bls_sigs_fails_if_less_than_n_sigs() {
        // number of nodes
        const n: usize = 10;

        let mut rng = ChaCha20Rng::from_entropy();
        let mut msgs: Vec<Vec<u8>> = vec![vec![]; n];
        for i in 0..n {
            let msg_len = (rng.next_u64() & 0x3F) + 1;
            msgs[i] = vec![0u8; msg_len as usize];
            rng.fill_bytes(&mut msgs[i]);
            println!("msg: {:?}", msgs[i]);
        }

        let msgs_refs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();

        let (agg_pk, sks) = dist_bls_key_gen(n).unwrap();

        // derive n pks
        let pks: Vec<PublicKey> = sks.iter().map(|sk| {
            let pk = sk.sk_to_pk();
            // println!("pk: {:?}", hex::encode(pk.to_bytes()));
            pk
        }).collect();
        let pks_refs: Vec<&PublicKey> = pks.iter().map(|pk| pk) .collect();

        // each node signs different msg
        let sigs: Vec<Signature> = sks
            .iter()
            .zip(msgs.clone().into_iter())
            .map(|(sk, msg)| {
            let sig = sk.sign(msg.as_slice(), CIPHER_SUITE, &[]);
            println!("sig: {:?}", hex::encode(sig.to_bytes()));
            sig
        }).collect();
        let mut sigs_refs = sigs.iter().map(|s| s).collect::<Vec<&Signature>>();

        // Drop the last signature to force a failure
        sigs_refs.truncate(n - 1);
        assert_eq!(sigs_refs.len(), n - 1);

        // should panic
        aggregate_non_uniform_bls_sigs(sigs_refs, pks_refs, msgs_refs).unwrap();
    }

    #[test]
    fn test_distribute_encrypted_bls_keys() -> Result<()> {
        // number of nodes
        const n: usize = 10;

        // generate n eth keys
        let eth_pks: Vec<EthPublicKey> = (0..n).into_iter()
            .map(|_| eth_key_gen().unwrap()).collect();

        // derive n eth wallet addresses
        let eth_addrs: Vec<String> = eth_pks.iter()
            .map(|pk| pk_to_eth_addr(pk).unwrap()).collect();

        // lookup n eth secret keys
        let eth_sks: Vec<EthSecretKey> = eth_addrs.iter()
            .map(|addr| read_eth_key(addr).unwrap()).collect();

        // generate n BLS keys
        let (agg_pk, bls_sks) = dist_bls_key_gen(n)?;

        // encrypt each bls sk
        let ct_bls_sks: Vec<Vec<u8>> = eth_pks
            .iter()
            .zip(bls_sks.iter())
            .map(|(eth_pk, bls_sk)| {
                encrypt(&eth_pk.serialize(), &bls_sk.serialize()).expect("Could not encrpyt bls sk")
            }).collect();

        // decrypt each encrypted bls sk
        let pt_bls_sks: Vec<SecretKey> = eth_sks
            .iter()
            .zip(ct_bls_sks.iter())
            .map(|(eth_sk, ct_bls_sk)| {
                let sk_bytes = decrypt(&eth_sk.serialize(), &ct_bls_sk).expect("Could not encrpyt bls sk");
                SecretKey::from_bytes(&sk_bytes).expect("couldnt convert to BLS key")
            }).collect();
        
        // verify we decrypted the correct BLS secret key
        pt_bls_sks
            .iter()
            .zip(bls_sks.iter())
            .for_each(|(sk_got, sk_exp)| {
                assert_eq!(sk_got.serialize(), sk_exp.serialize())
            });
        Ok(())
    }
}