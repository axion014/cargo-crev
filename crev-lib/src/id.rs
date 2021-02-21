use crate::{Error, Result};
use argon2::{self, Config};
use crev_common::{
    rand::random_vec,
    serde::{as_base64, from_base64},
};
use crev_data::id::{PublicId, UnlockedId};
use serde::{Deserialize, Serialize};
use std::{self, fmt, fs, io, io::Write, path::Path};

const CURRENT_LOCKED_ID_SERIALIZATION_VERSION: i64 = -1;
pub type PassphraseFn<'a> = &'a dyn Fn() -> std::io::Result<String>;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PassphraseConfig {
    version: u32,
    variant: String,
    iterations: u32,
    #[serde(rename = "memory-size")]
    memory_size: u32,
    lanes: Option<u32>,
    #[serde(serialize_with = "as_base64", deserialize_with = "from_base64")]
    salt: Vec<u8>,
}

/// Serialized, stored on disk
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LockedId {
    version: i64,
    #[serde(flatten)]
    pub url: crev_data::Url,
    #[serde(serialize_with = "as_base64", deserialize_with = "from_base64")]
    #[serde(rename = "public-key")]
    pub public_key: Vec<u8>,
    #[serde(serialize_with = "as_base64", deserialize_with = "from_base64")]
    #[serde(rename = "sealed-secret-key")]
    sealed_secret_key: Vec<u8>,

    #[serde(serialize_with = "as_base64", deserialize_with = "from_base64")]
    #[serde(rename = "seal-nonce")]
    seal_nonce: Vec<u8>,

    #[serde(rename = "pass")]
    passphrase_config: PassphraseConfig,
}

impl fmt::Display for LockedId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // https://github.com/dtolnay/serde-yaml/issues/103
        f.write_str(&serde_yaml::to_string(self).map_err(|_| fmt::Error)?)
    }
}

impl std::str::FromStr for LockedId {
    type Err = serde_yaml::Error;

    fn from_str(yaml_str: &str) -> std::result::Result<Self, Self::Err> {
        Ok(serde_yaml::from_str::<LockedId>(&yaml_str)?)
    }
}

impl LockedId {
    pub fn from_unlocked_id(unlocked_id: &UnlockedId, passphrase: &str) -> Result<LockedId> {
        use miscreant::aead::Aead;

        let config = Config {
            variant: argon2::Variant::Argon2id,
            version: argon2::Version::Version13,

            hash_length: 64,
            mem_cost: 4096,
            time_cost: 192,

            lanes: num_cpus::get() as u32,
            thread_mode: argon2::ThreadMode::Parallel,

            ad: &[],
            secret: &[],
        };

        let pwsalt = random_vec(32);
        let pwhash =
            argon2::hash_raw(passphrase.as_bytes(), &pwsalt, &config).map_err(Error::Passphrase)?;

        let mut siv = miscreant::aead::Aes256SivAead::new(&pwhash);

        let seal_nonce = random_vec(32);

        Ok(LockedId {
            version: CURRENT_LOCKED_ID_SERIALIZATION_VERSION,
            public_key: unlocked_id.keypair.public.to_bytes().to_vec(),
            sealed_secret_key: siv.seal(&seal_nonce, &[], unlocked_id.keypair.secret.as_bytes()),
            seal_nonce,
            url: unlocked_id.url().clone(),
            passphrase_config: PassphraseConfig {
                salt: pwsalt,
                iterations: config.time_cost,
                memory_size: config.mem_cost,
                version: 0x13,
                lanes: Some(config.lanes),
                variant: config.variant.as_lowercase_str().to_string(),
            },
        })
    }

    /// Extract only the public identity part from all data
    pub fn to_public_id(&self) -> PublicId {
        PublicId::new_from_pubkey(self.public_key.to_owned(), self.url.clone())
            .expect("Invalid locked id.")
    }

    pub fn pub_key_as_base64(&self) -> String {
        crev_common::base64_encode(&self.public_key)
    }

    /// Write the Id to this file, overwriting it
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let s = self.to_string();
        crev_common::store_str_to_file(path, &s)
            .map_err(|e| Error::FileWrite(e, path.into()))
    }

    fn _replace_file(&self, path: &Path) -> io::Result<()> {
        let mut file = fs::File::create(path)?;

        // it is not terribly important for this file to be readable
        // only for the user (because the key is encrypted anyway),
        // so ignore the error if it happens
        let _ = crate::util::chmod_path_to_600(path);

        write!(file, "{}", self)?;

        Ok(())
    }

    pub fn read_from_yaml_file(path: &Path) -> Result<Self> {
        let file = std::fs::File::open(path)?;

        Ok(serde_yaml::from_reader(&file)?)
    }

    pub fn to_unlocked(&self, passphrase: &str) -> Result<UnlockedId> {
        let LockedId {
            ref version,
            ref url,
            ref public_key,
            ref sealed_secret_key,
            ref seal_nonce,
            ref passphrase_config,
        } = self;
        {
            if *version > CURRENT_LOCKED_ID_SERIALIZATION_VERSION {
                Err(Error::UnsupportedVersion(*version))?;
            }
            use miscreant::aead::Aead;

            let mut config = Config {
                variant: argon2::Variant::from_str(&passphrase_config.variant)?,
                version: argon2::Version::Version13,

                hash_length: 64,
                mem_cost: passphrase_config.memory_size,
                time_cost: passphrase_config.iterations,

                lanes: num_cpus::get() as u32,
                thread_mode: argon2::ThreadMode::Parallel,

                ad: &[],
                secret: &[],
            };

            if let Some(lanes) = passphrase_config.lanes {
                config.lanes = lanes;
            } else {
                eprintln!(
                    "`lanes` not configured. Old bug. See: https://github.com/crev-dev/cargo-crev/issues/151"
                );
                eprintln!("Using `lanes: {}`", config.lanes);
            }

            let passphrase_hash =
                argon2::hash_raw(passphrase.as_bytes(), &passphrase_config.salt, &config)?;
            let mut siv = miscreant::aead::Aes256SivAead::new(&passphrase_hash);

            let secret_key = siv
                .open(&seal_nonce, &[], &sealed_secret_key)
                .map_err(|_| Error::IncorrectPassphrase)?;

            assert!(!secret_key.is_empty());

            let result = UnlockedId::new(url.to_owned(), secret_key)?;
            if public_key != &result.keypair.public.to_bytes() {
                Err(Error::PubKeyMismatch)?;
            }
            Ok(result)
        }
    }
}
