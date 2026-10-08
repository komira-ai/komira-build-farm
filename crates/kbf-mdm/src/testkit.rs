//! Test material: security-key signatures made in software, as a FIDO2 key would make
//! them (OpenSSH's PROTOCOL.u2f), so tests can set the flags byte a real key sets.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use ssh_key::private::Ed25519Keypair;
use ssh_key::public::{Ed25519PublicKey, KeyData, SkEd25519};
use ssh_key::{Algorithm, HashAlg, LineEnding, PrivateKey, PublicKey, Signature, SshSig};

use crate::signers::{FLAG_USER_PRESENT, NAMESPACE};

/// An `sk-ssh-ed25519@openssh.com` key whose private half the test holds.
pub struct SkKey {
    signing: SigningKey,
    public: PublicKey,
}

impl SkKey {
    pub fn new(seed: u8) -> Self {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let point = Ed25519PublicKey(signing.verifying_key().to_bytes());
        let public = PublicKey::from(KeyData::SkEd25519(SkEd25519::new(point, "ssh:")));
        Self { signing, public }
    }

    /// The key in OpenSSH public-key form.
    pub fn openssh(&self) -> String {
        self.public.to_openssh().unwrap()
    }

    /// An allowed-signers line for this key, limited to the gate's namespace.
    pub fn allowed_line(&self, principal: &str) -> String {
        format!("{principal} namespaces=\"{NAMESPACE}\" {}", self.openssh())
    }

    /// Signs `message` in `namespace` with the given flags byte, as
    /// `ssh-keygen -Y sign` with this security key would.
    pub fn sign_with(&self, namespace: &str, message: &[u8], flags: u8) -> String {
        let signed = SshSig::signed_data(namespace, HashAlg::Sha512, message).unwrap();
        let counter = 7u32.to_be_bytes();
        let mut inner = Sha256::digest(b"ssh:").to_vec();
        inner.push(flags);
        inner.extend(counter);
        inner.extend(Sha256::digest(&signed));
        let mut data = self.signing.sign(&inner).to_bytes().to_vec();
        data.push(flags);
        data.extend(counter);
        let sig = Signature::new(Algorithm::SkEd25519, data).unwrap();
        SshSig::new(
            self.public.key_data().clone(),
            namespace,
            HashAlg::Sha512,
            sig,
        )
        .unwrap()
        .to_pem(LineEnding::LF)
        .unwrap()
    }

    /// A touched signature in the gate's namespace.
    pub fn sign(&self, message: &[u8]) -> String {
        self.sign_with(NAMESPACE, message, FLAG_USER_PRESENT)
    }
}

/// A fresh scratch directory for one test, beside the test binary.
pub fn scratch(name: &str) -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    let dir = exe.parent().unwrap().join("kbf-mdm-unit").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A software `ssh-ed25519` key: its allowed-signers line and its signature over
/// `message` in the gate's namespace.
pub fn software_signature(principal: &str, message: &[u8]) -> (String, String) {
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&[42; 32]));
    let line = format!(
        "{principal} namespaces=\"{NAMESPACE}\" {}",
        key.public_key().to_openssh().unwrap()
    );
    let sig = SshSig::sign(&key, NAMESPACE, HashAlg::Sha512, message)
        .unwrap()
        .to_pem(LineEnding::LF)
        .unwrap();
    (line, sig)
}
