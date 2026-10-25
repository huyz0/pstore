//! Ephemeral test CA material, unrelated to deployment identities.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test fixture construction reports failure by panic"
)]
use rcgen::{
    BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};

pub(crate) struct Authority {
    pub(crate) pem: String,
    issuer: Issuer<'static, KeyPair>,
}

pub(crate) struct Material {
    pub(crate) ca: String,
    pub(crate) cert: String,
    pub(crate) key: String,
}

impl Authority {
    pub(crate) fn new() -> Self {
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2045, 1, 1);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        Self {
            pem: cert.pem(),
            issuer: Issuer::new(params, key),
        }
    }

    pub(crate) fn issue(&self, names: &[&str], expired: bool) -> Material {
        let mut params =
            CertificateParams::new(names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>())
                .unwrap();
        params.extended_key_usages = vec![
            ExtendedKeyUsagePurpose::ClientAuth,
            ExtendedKeyUsagePurpose::ServerAuth,
        ];
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(if expired { 2021 } else { 2045 }, 1, 1);
        let key = KeyPair::generate().unwrap();
        let cert = params.signed_by(&key, &self.issuer).unwrap();
        Material {
            ca: self.pem.clone(),
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }
}
