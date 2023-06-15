use crate::{PublicKey, SecretKey};

/// Used when constructing a `Deploy`.
#[derive(Debug)]
pub(in crate::deploy) enum AccountAndSecretKey<'a> {
    /// Provides both the account and the secret key (not necessarily for the same account) used to
    /// sign the `Deploy`.
    Both {
        /// The public key of the account.
        account: PublicKey,
        /// The secret key used to sign the `Deploy`.
        secret_key: &'a SecretKey,
    },
    /// The public key of the account.  The `Deploy` will be created unsigned as no secret key is
    /// provided.
    Account(PublicKey),
    /// The account will be derived from the provided secret key, and the `Deploy` will be signed
    /// by the same secret key.
    SecretKey(&'a SecretKey),
}

impl<'a> AccountAndSecretKey<'a> {
    pub fn account(&self) -> PublicKey {
        match self {
            AccountAndSecretKey::Both { account, .. } | AccountAndSecretKey::Account(account) => {
                account.clone()
            }
            AccountAndSecretKey::SecretKey(secret_key) => PublicKey::from(*secret_key),
        }
    }

    pub fn secret_key(&self) -> Option<&SecretKey> {
        match self {
            AccountAndSecretKey::Both { secret_key, .. }
            | AccountAndSecretKey::SecretKey(secret_key) => Some(secret_key),
            AccountAndSecretKey::Account(_) => None,
        }
    }
}
