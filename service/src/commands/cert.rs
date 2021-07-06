/*
 * Copyright (c)2013-2021 ZeroTier, Inc.
 *
 * Use of this software is governed by the Business Source License included
 * in the LICENSE.TXT file in the project's root directory.
 *
 * Change Date: 2026-01-01
 *
 * On the date above, in accordance with the Business Source License, use
 * of this software will be governed by version 2.0 of the Apache License.
 */
/****/

use std::str::FromStr;
use std::sync::Arc;

use clap::ArgMatches;
use dialoguer::Input;
use zerotier_core::*;

use crate::store::Store;
use crate::utils::{read_limit, ms_since_epoch, to_json_pretty};
use crate::GlobalFlags;

fn cert_usage_flags_to_string(flags: u64) -> String {
    let mut flags_str = String::new();
    for f in [
        (CERTIFICATE_USAGE_DIGITAL_SIGNATURE, "s"),
        (CERTIFICATE_USAGE_NON_REPUDIATION, "n"),
        (CERTIFICATE_USAGE_KEY_ENCIPHERMENT, "e"),
        (CERTIFICATE_USAGE_DATA_ENCIPHERMENT, "d"),
        (CERTIFICATE_USAGE_KEY_AGREEMENT, "a"),
        (CERTIFICATE_USAGE_CERTIFICATE_SIGNING, "c"),
        (CERTIFICATE_USAGE_CRL_SIGNING, "r"),
        (CERTIFICATE_USAGE_EXECUTABLE_SIGNATURE, "x"),
        (CERTIFICATE_USAGE_TIMESTAMPING, "t"),
        (CERTIFICATE_USAGE_ZEROTIER_ROOT_SET, "z"),
    ].iter() {
        if (flags & (*f).0) != 0 {
            if !flags_str.is_empty() {
                flags_str.push(',');
            }
            flags_str.push_str((*f).1);
        }
    }
    flags_str
}

fn cert_string_to_usage_flags(flags_str: &str) -> u64 {
    let mut flags: u64 = 0;
    for c in flags_str.chars().into_iter() {
        flags |= match c {
            's' => CERTIFICATE_USAGE_DIGITAL_SIGNATURE,
            'n' => CERTIFICATE_USAGE_NON_REPUDIATION,
            'e' => CERTIFICATE_USAGE_KEY_ENCIPHERMENT,
            'd' => CERTIFICATE_USAGE_DATA_ENCIPHERMENT,
            'a' => CERTIFICATE_USAGE_KEY_AGREEMENT,
            'c' => CERTIFICATE_USAGE_CERTIFICATE_SIGNING,
            'r' => CERTIFICATE_USAGE_CRL_SIGNING,
            'x' => CERTIFICATE_USAGE_EXECUTABLE_SIGNATURE,
            't' => CERTIFICATE_USAGE_TIMESTAMPING,
            'z' => CERTIFICATE_USAGE_ZEROTIER_ROOT_SET,
            _ => 0,
        }
    }
    flags
}

fn cert_print(certificate: &Certificate) {
    let mut subject_identities = String::new();
    let mut subject_networks = String::new();
    let mut subject_update_urls = String::new();
    let mut usage_flags = String::new();

    fn string_or_dash(s: &str) -> &str {
        if s.is_empty() {
            "-"
        } else {
            s
        }
    }

    if certificate.subject.identities.is_empty() {
        subject_identities.push_str(":           (none)");
    } else {
        for x in certificate.subject.identities.iter() {
            subject_identities.push_str("\n    ");
            subject_identities.push_str(x.identity.to_string().as_str());
            if x.locator.is_some() {
                subject_identities.push_str(" ");
                subject_identities.push_str(x.locator.as_ref().unwrap().to_string().as_str());
            }
        }
    }

    if certificate.subject.networks.is_empty() {
        subject_networks.push_str(":             (none)");
    } else {
        for x in certificate.subject.networks.iter() {
            subject_networks.push_str("\n    ");
            subject_networks.push_str(x.id.to_string().as_str());
            if x.controller.is_some() {
                subject_networks.push_str(" at ");
                subject_networks.push_str(x.controller.as_ref().unwrap().to_string().as_str());
            }
        }
    }

    if certificate.subject.update_urls.is_empty() {
        subject_update_urls.push_str(":          (none)");
    } else {
        for x in certificate.subject.update_urls.iter() {
            subject_update_urls.push_str("\n    ");
            subject_update_urls.push_str(x.as_ref());
        }
    }

    if certificate.usage_flags != 0 {
        usage_flags.push_str(" (");
        usage_flags.push_str(cert_usage_flags_to_string(certificate.usage_flags).as_str());
        usage_flags.push(')');
    }

    println!(r###"Serial Number (SHA384): {}
Usage Flags:            0x{:0>8x}{}
Timestamp:              {}
Validity:               {} to {}
Subject
  Timestamp:            {}
  Identities{}
  Networks{}
  Update URLs{}
  Name
    Serial:             {}
    Common Name:        {}
    Country:            {}
    Organization:       {}
    Unit:               {}
    Locality:           {}
    State/Province:     {}
    Street Address:     {}
    Postal Code:        {}
    E-Mail:             {}
    URL:                {}
    Host:               {}
  Unique ID:            {}
  Unique ID Signature:  {}
Issuer:                 {}
Issuer Public Key:      {}
Public Key:             {}
Extended Attributes:    {} bytes
Signature:              {}
Maximum Path Length:    {}{}"###,
             certificate.serial_no.to_string(),
             certificate.usage_flags, usage_flags,
             certificate.timestamp,
             certificate.validity[0], certificate.validity[1],
             certificate.subject.timestamp,
             subject_identities,
             subject_networks,
             subject_update_urls,
             string_or_dash(certificate.subject.name.serial_no.as_str()),
             string_or_dash(certificate.subject.name.common_name.as_str()),
             string_or_dash(certificate.subject.name.country.as_str()),
             string_or_dash(certificate.subject.name.organization.as_str()),
             string_or_dash(certificate.subject.name.unit.as_str()),
             string_or_dash(certificate.subject.name.locality.as_str()),
             string_or_dash(certificate.subject.name.province.as_str()),
             string_or_dash(certificate.subject.name.street_address.as_str()),
             string_or_dash(certificate.subject.name.postal_code.as_str()),
             string_or_dash(certificate.subject.name.email.as_str()),
             string_or_dash(certificate.subject.name.url.as_str()),
             string_or_dash(certificate.subject.name.host.as_str()),
             string_or_dash(base64_encode(&certificate.subject.unique_id).as_str()),
             string_or_dash(base64_encode(&certificate.subject.unique_id_signature).as_str()),
             certificate.issuer.to_string(),
             string_or_dash(base64_encode(&certificate.issuer_public_key).as_str()),
             string_or_dash(base64_encode(&certificate.public_key).as_str()),
             certificate.extended_attributes.len(),
             string_or_dash(base64_encode(&certificate.signature).as_str()),
             certificate.max_path_length, if certificate.max_path_length == 0 { " (leaf)" } else { " (CA or sub-CA)" });
}

fn list(store: &Arc<Store>) -> i32 {
    0
}

fn show<'a>(store: &Arc<Store>, global_flags: &GlobalFlags, cli_args: &ArgMatches<'a>) -> i32 {
    let serial_or_path = cli_args.value_of("serialorpath").unwrap().trim();
    CertificateSerialNo::new_from_string(serial_or_path).map_or_else(|| {
        read_limit(serial_or_path, 65536).map_or_else(|e| {
            println!("ERROR: unable to read certificate from '{}': {}", serial_or_path, e.to_string());
            1
        }, |cert_json| {
            serde_json::from_slice::<Certificate>(cert_json.as_ref()).map_or_else(|e| {
                println!("ERROR: unable to decode certificate from '{}': {}", serial_or_path, e.to_string());
                1
            }, |certificate| {
                if global_flags.json_output {
                    println!("{}", to_json_pretty(&certificate));
                } else {
                    cert_print(&certificate);
                }
                let cv = certificate.verify(ms_since_epoch());
                if cv != CertificateError::None {
                    println!("\nWARNING: certificate validity check failed: {}", cv.to_str());
                }
                0
            })
        })
    }, |serial| {
        // TODO: query node
        0
    })
}

fn newsuid(cli_args: Option<&ArgMatches>) -> i32 {
    let key_pair = Certificate::new_key_pair(CertificatePublicKeyAlgorithm::ECDSANistP384);
    if key_pair.is_err() {
        println!("ERROR: internal error creating key pair: {}", key_pair.err().unwrap().to_str());
        1
    } else {
        let (_, privk) = key_pair.ok().unwrap();
        let privk_base64 = base64_encode(&privk);
        let path = cli_args.map_or("", |cli_args| { cli_args.value_of("path").unwrap_or("") });
        if path.is_empty() {
            println!("{}", privk_base64);
            0
        } else {
            std::fs::write(path, privk_base64.as_bytes()).map_or_else(|e| {
                eprintln!("FATAL: error writing '{}': {}", path, e.to_string());
                e.raw_os_error().unwrap_or(1)
            }, |_| {
                println!("Subject unique ID secret written to: {} (public is included)", path);
                0
            })
        }
    }
}

fn newcsr(cli_args: &ArgMatches) -> i32 {
    let theme = &dialoguer::theme::SimpleTheme;

    let subject_unique_id: String = Input::with_theme(theme)
        .with_prompt("Path to subject unique ID secret key (empty to create unsigned subject)")
        .allow_empty(true)
        .interact_text()
        .unwrap_or_default();
    let subject_unique_id_private_key = if subject_unique_id.is_empty() {
        None
    } else {
        let b = crate::utils::read_limit(subject_unique_id, 1024);
        if b.is_err() {
            println!("ERROR: unable to read subject unique ID secret file: {}", b.err().unwrap().to_string());
            return 1;
        }
        let privk_encoded = String::from_utf8(b.unwrap());
        if privk_encoded.is_err() {
            println!("ERROR: invalid UTF-8 in secret");
            return 1;
        }
        let privk_encoded = privk_encoded.unwrap();
        let privk = base64_decode(&privk_encoded);
        if privk.is_err() || privk.as_ref().unwrap().is_empty() {
            println!("ERROR: invalid unique ID secret: {}", privk.err().unwrap().to_string());
            return 1;
        }
        Some(privk.unwrap())
    };

    let timestamp: i64 = Input::with_theme(theme)
        .with_prompt("Subject timestamp (seconds since epoch)")
        .with_initial_text((crate::utils::ms_since_epoch() / 1000).to_string())
        .allow_empty(false)
        .interact_text()
        .unwrap_or(0);
    if timestamp < 0 {
        println!("ERROR: invalid timestamp");
        return 1;
    }

    println!("Subject identities");
    let mut identities: Vec<CertificateIdentity> = Vec::new();
    loop {
        let identity: String = Input::with_theme(theme)
            .with_prompt(format!("  [{}] Identity or path to identity (empty to end)", identities.len() + 1))
            .allow_empty(true)
            .interact_text()
            .unwrap_or_default();
        if identity.is_empty() {
            break;
        }
        let identity = crate::utils::read_identity(identity.as_str(), true);
        if identity.is_err() {
            println!("ERROR: identity invalid or unable to read from file.");
            return 1;
        }
        let identity = identity.unwrap();
        if identity.has_private() {
            println!("ERROR: identity contains private key, use public only for CSR!");
            return 1;
        }

        let locator: String = Input::with_theme(theme)
            .with_prompt(format!("  [{}] Locator or path to locator for {} (optional)", identities.len() + 1, identity.address.to_string()))
            .allow_empty(true)
            .interact_text()
            .unwrap_or_default();
        let locator = if locator.is_empty() {
            None
        } else {
            let l = crate::utils::read_locator(locator.as_str());
            if l.is_err() {
                println!("ERROR: locator invalid: {}", l.err().unwrap());
                return 1;
            }
            let l = l.ok();
            if !l.as_ref().unwrap().verify(&identity) {
                println!("ERROR: locator was not signed by this identity.");
                return 1;
            }
            l
        };

        identities.push(CertificateIdentity {
            identity,
            locator,
        });
    }

    println!("Subject networks (empty to end)");
    let mut networks: Vec<CertificateNetwork> = Vec::new();
    loop {
        let nwid: String = Input::with_theme(theme)
            .with_prompt(format!("  [{}] Network ID (empty to end)", networks.len() + 1))
            .allow_empty(true)
            .interact_text()
            .unwrap_or_default();
        if nwid.len() != 16 {
            break;
        }
        let nwid = NetworkId::from(nwid.as_str());

        let fingerprint: String = Input::with_theme(theme)
            .with_prompt(format!("  [{}] Fingerprint of primary controller (optional)", networks.len() + 1))
            .allow_empty(true)
            .interact_text()
            .unwrap_or_default();
        let fingerprint = if fingerprint.is_empty() {
            None
        } else {
            let f = Fingerprint::new_from_string(fingerprint.as_str());
            if f.is_err() {
                println!("ERROR: fingerprint invalid: {}", f.err().unwrap().to_str());
                return 1;
            }
            f.ok()
        };

        networks.push(CertificateNetwork {
            id: nwid,
            controller: fingerprint,
        })
    }

    println!("Subject certificate update URLs");
    let mut update_urls: Vec<String> = Vec::new();
    loop {
        let url: String = Input::with_theme(theme)
            .with_prompt(format!("  [{}] URL (empty to end)", update_urls.len() + 1))
            .allow_empty(true)
            .interact_text()
            .unwrap_or_default();
        if url.is_empty() {
            break;
        }
        let url_parsed = hyper::Uri::from_str(url.as_str());
        if url_parsed.is_err() {
            println!("ERROR: invalid URL: {}", url_parsed.err().unwrap().to_string());
            return 1;
        }
        update_urls.push(url);
    }

    println!("Certificate name information (all fields are optional)");
    let name = CertificateName {
        serial_no: Input::with_theme(theme).with_prompt("  Serial (user-defined)").allow_empty(true).interact_text().unwrap_or_default(),
        common_name: Input::with_theme(theme).with_prompt("  Common Name").allow_empty(true).interact_text().unwrap_or_default(),
        organization: Input::with_theme(theme).with_prompt("  Organization").allow_empty(true).interact_text().unwrap_or_default(),
        unit: Input::with_theme(theme).with_prompt("  Organizational Unit").allow_empty(true).interact_text().unwrap_or_default(),
        country: Input::with_theme(theme).with_prompt("  Country").allow_empty(true).interact_text().unwrap_or_default(),
        province: Input::with_theme(theme).with_prompt("  State/Province").allow_empty(true).interact_text().unwrap_or_default(),
        locality: Input::with_theme(theme).with_prompt("  Locality").allow_empty(true).interact_text().unwrap_or_default(),
        street_address: Input::with_theme(theme).with_prompt("  Street Address").allow_empty(true).interact_text().unwrap_or_default(),
        postal_code: Input::with_theme(theme).with_prompt("  Postal Code").allow_empty(true).interact_text().unwrap_or_default(),
        email: Input::with_theme(theme).with_prompt("  E-Mail").allow_empty(true).interact_text().unwrap_or_default(),
        url: Input::with_theme(theme).with_prompt("  URL (informational)").allow_empty(true).interact_text().unwrap_or_default(),
        host: Input::with_theme(theme).with_prompt("  Host").allow_empty(true).interact_text().unwrap_or_default(),
    };

    let subject = CertificateSubject {
        timestamp,
        identities,
        networks,
        update_urls,
        name,
        unique_id: Vec::new(),
        unique_id_signature: Vec::new(),
    };

    let (_, privk) = Certificate::new_key_pair(CertificatePublicKeyAlgorithm::ECDSANistP384).ok().unwrap();
    subject.new_csr(privk.as_ref(), subject_unique_id_private_key.as_ref().map(|k| k.as_ref())).map_or_else(|e| {
        println!("ERROR: error creating CRL: {}", e.to_str());
        1
    }, |csr| {
        let csr_path = cli_args.value_of("csrpath").unwrap();
        std::fs::write(csr_path, csr).map_or_else(|e| {
            println!("ERROR: unable to write CSR: {}", e.to_string());
            1
        }, |_| {
            let secret_path = cli_args.value_of("secretpath").unwrap();
            std::fs::write(secret_path, base64_encode(&privk)).map_or_else(|e| {
                let _ = std::fs::remove_file(csr_path);
                println!("ERROR: unable to write secret: {}", e.to_string());
                1
            }, |_| {
                println!("CSR written to '{}', certificate secret to '{}'", csr_path, secret_path);
                0
            })
        })
    })
}

fn sign<'a>(store: &Arc<Store>, cli_args: &ArgMatches<'a>) -> i32 {
    let now = ms_since_epoch();
    let csr_path = cli_args.value_of("csr").unwrap().trim(); // required
    let certout_path = cli_args.value_of("certout").unwrap().trim(); // required
    let usage = cert_string_to_usage_flags(cli_args.value_of("usage").unwrap_or(""));
    let timestamp = cli_args.value_of("timestamp").map_or_else(|| now, |ts| i64::from_str(ts.trim()).unwrap_or(now));
    let start_time = cli_args.value_of("start").map_or_else(|| timestamp, |s| i64::from_str(s.trim()).unwrap_or(now));
    let end_time = cli_args.value_of("ttl").map_or(i64::MAX, |e| i64::from_str(e.trim()).map_or(i64::MAX, |e| timestamp + e));
    let issuer_path = cli_args.value_of("issuer").unwrap().trim(); // required
    let issuer_secret_path = cli_args.value_of("issuersecret").unwrap().trim(); // required

    let csr = crate::utils::read_limit(csr_path, 131072);
    if csr.is_err() {
        println!("ERROR: unable to read CSR from '{}': {}", csr_path, csr.err().unwrap().to_string());
        return 1;
    }
    let csr = csr.unwrap();

    let cert = Certificate::new_from_bytes(csr.as_slice(), false);
    if cert.is_err() {
        println!("ERROR: error decoding CSR read from '{}': {}", csr_path, cert.err().unwrap().to_str());
        return 1;
    }
    let mut cert = cert.ok().unwrap();

    cert.usage_flags = usage;
    cert.timestamp = timestamp;
    cert.validity[0] = start_time;
    cert.validity[1] = end_time;

    let issuer = if issuer_path == "self" {
        cert.issuer_public_key.clear();
        cert.subject_signature.clear();
        cert.issuer.clone()
    } else {
        let issuer = crate::utils::read_limit(issuer_path, 131072);
        if issuer.is_err() {
            println!("ERROR: unable to read issuer from '{}': {}", issuer_path, issuer.err().unwrap().to_string());
            return 1;
        }
        let issuer = issuer.unwrap();

        let issuer_cert = Certificate::new_from_bytes(issuer.as_slice(), true);
        if issuer_cert.is_err() {
            println!("ERROR: issuer at '{}' is invalid: {}", issuer_path, issuer_cert.err().unwrap().to_str());
            return 1;
        }
        let issuer_cert = issuer_cert.ok().unwrap();

        if (issuer_cert.usage_flags & CERTIFICATE_USAGE_CERTIFICATE_SIGNING) == 0 {
            println!("ERROR: issuer at '{}' usage does not permit certificate signing.", issuer_path);
            return 1;
        }

        issuer_cert.issuer.clone()
    };

    let issuer_secret = crate::utils::read_limit(issuer_secret_path, 1024);
    if issuer_secret.is_err() {
        println!("ERROR: unable to read issuer secret from '{}': {}", issuer_secret_path, issuer_secret.err().unwrap().to_string());
        return 1;
    }
    let issuer_secret = issuer_secret.unwrap();
    let issuer_secret = base64_decode(&issuer_secret);
    if issuer_secret.is_err() {
        println!("ERROR: invalid issuer secret in '{}': invalid base64: {}", issuer_secret_path, issuer_secret.err().unwrap().to_string());
        return 1;
    }
    let issuer_secret = issuer_secret.unwrap();

    let signed_cert = cert.sign(&issuer, issuer_secret.as_slice());
    if signed_cert.is_err() {
        println!("ERROR: error signing certificate: {}", signed_cert.err().unwrap().to_str());
        return 1;
    }
    let signed_cert = signed_cert.ok().unwrap();

    let signed_cert_check = signed_cert.verify(-1);
    if signed_cert_check != CertificateError::None {
        println!("ERROR: error signing certificate: failed verify after sign: {}", signed_cert_check.to_str());
        return 1;
    }

    cert_print(&signed_cert);
    println!();

    let signed_cert = signed_cert.to_bytes().ok().unwrap();
    let signed_write_result = std::fs::write(certout_path, signed_cert.as_slice());
    if signed_write_result.is_err() {
        println!("ERROR: unable to write result to '{}': {}", certout_path, signed_write_result.err().unwrap().to_string());
        return 1;
    }
    println!("Signed certificate written to {}", certout_path);
    0
}

fn verify<'a>(store: &Arc<Store>, cli_args: &ArgMatches<'a>) -> i32 {
    0
}

fn import<'a>(store: &Arc<Store>, cli_args: &ArgMatches<'a>) -> i32 {
    0
}

fn factoryreset(store: &Arc<Store>) -> i32 {
    0
}

fn export<'a>(store: &Arc<Store>, cli_args: &ArgMatches<'a>) -> i32 {
    0
}

fn delete<'a>(store: &Arc<Store>, cli_args: &ArgMatches<'a>) -> i32 {
    0
}

pub(crate) fn run(store: Arc<Store>, global_flags: GlobalFlags, cli_args: &ArgMatches) -> i32 {
    match cli_args.subcommand() {
        ("list", None) => list(&store),
        ("show", Some(sub_cli_args)) => show(&store, &global_flags, sub_cli_args),
        ("newsuid", sub_cli_args) => newsuid(sub_cli_args),
        ("newcsr", Some(sub_cli_args)) => newcsr(sub_cli_args),
        ("sign", Some(sub_cli_args)) => sign(&store, sub_cli_args),
        ("verify", Some(sub_cli_args)) => verify(&store, sub_cli_args),
        ("import", Some(sub_cli_args)) => import(&store, sub_cli_args),
        ("export", Some(sub_cli_args)) => export(&store, sub_cli_args),
        ("delete", Some(sub_cli_args)) => delete(&store, sub_cli_args),
        ("factoryreset", None) => factoryreset(&store),
        _ => {
            crate::print_help(true);
            1
        }
    }
}
