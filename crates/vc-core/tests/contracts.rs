//! Contract tests for the frozen vc-core types (Task 0A, Step 1).

use proptest::prelude::*;
use vc_core::sources::SourceSpec;
use vc_core::usepolicy::{UsePolicy, UseSignals};
use vc_core::{CanonicalUrl, UrlKey};

fn url_strategy() -> impl Strategy<Value = String> {
    let scheme = prop_oneof![Just("http"), Just("https"), Just("HTTP"), Just("HtTpS")];
    let host = prop_oneof![
        "[a-z]{1,8}\\.(com|org|co\\.uk|travel)",
        "[A-Z]{1,5}\\.Example\\.COM",
        Just("127.0.0.1".to_string()),
        Just("0x7f.1".to_string()),
        Just("xn--bcher-kva.example".to_string()),
        Just("bücher.example".to_string()),
    ];
    let port = prop_oneof![
        Just(String::new()),
        Just(":80".into()),
        Just(":443".into()),
        Just(":8080".into())
    ];
    let seg = prop_oneof![
        "[a-zA-Z0-9_~-]{0,6}",
        Just(".".to_string()),
        Just("..".to_string()),
        Just("%7e".to_string()),
        Just("%2F".to_string()),
        Just("%2e%2E".to_string()),
        Just("a%20b".to_string()),
        Just("é".to_string()),
        Just("%zz".to_string()),
    ];
    let path = prop::collection::vec(seg, 0..5).prop_map(|v| {
        if v.is_empty() {
            String::new()
        } else {
            format!("/{}", v.join("/"))
        }
    });
    let pair = prop_oneof![
        "[a-z]{1,4}=[a-zA-Z0-9]{0,4}",
        Just("utm_source=x".to_string()),
        Just("FBCLID=1".to_string()),
        Just("q=%7e%3d".to_string()),
        Just("=x".to_string()),
        Just("flag".to_string()),
        Just(String::new()),
    ];
    let query = prop::option::of(prop::collection::vec(pair, 0..5).prop_map(|v| v.join("&")));
    let frag = prop::option::of("[a-z]{0,4}");
    (scheme, host, port, path, query, frag).prop_map(|(s, h, po, pa, q, f)| {
        let mut u = format!("{s}://{h}{po}{pa}");
        if let Some(q) = q {
            u.push('?');
            u.push_str(&q);
        }
        if let Some(f) = f {
            u.push('#');
            u.push_str(&f);
        }
        u
    })
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    #[test]
    fn canonical_url_is_idempotent(raw in url_strategy()) {
        if let Ok(c1) = CanonicalUrl::parse(&raw) {
            let c2 = CanonicalUrl::parse(c1.as_str()).expect("canonical form must re-parse");
            prop_assert_eq!(c1.as_str(), c2.as_str());
            prop_assert_eq!(c1.key(), c2.key());
        }
    }

    #[test]
    fn use_policy_meet_only_restricts(a in any::<[bool; 6]>(), b in any::<[bool; 6]>()) {
        let sig = |x: [bool; 6]| UseSignals {
            noindex: x[0], noarchive: x[1], tdm_reserved: x[2],
            ai_input_no: x[3], ai_bot_blocked: x[4], ai_train_no: x[5],
        };
        let (sa, sb) = (sig(a), sig(b));
        let (pa, pb) = (UsePolicy::from_signals(&sa), UsePolicy::from_signals(&sb));
        let m = pa.meet(&pb);
        prop_assert!(m.is_at_most(&pa));
        prop_assert!(m.is_at_most(&pb));
        prop_assert!(m <= pa && m <= pb);
        // Commutative, idempotent, and a homomorphism from signal union.
        prop_assert_eq!(m, pb.meet(&pa));
        prop_assert_eq!(pa.meet(&pa), pa);
        prop_assert_eq!(UsePolicy::from_signals(&sa.union(&sb)), m);
        // Every policy is below ALLOW_ALL and above DENY_ALL.
        prop_assert!(pa.is_at_most(&UsePolicy::ALLOW_ALL));
        prop_assert!(UsePolicy::DENY_ALL.is_at_most(&pa));
    }
}

#[test]
fn url_key_is_16_bytes_and_stable() {
    // blake3("https://example.com/")[..16], computed independently with the Python `blake3`
    // package. Changing this breaks every stored url_key.
    let k = UrlKey::of("https://example.com/");
    assert_eq!(k.as_bytes().len(), 16);
    assert_eq!(k.to_hex(), "19f876713c227a568d939946496a0d30");
    assert_eq!(
        CanonicalUrl::parse("HTTPS://EXAMPLE.com:443")
            .unwrap()
            .key(),
        k
    );
    let s = serde_json::to_string(&k).unwrap();
    assert_eq!(serde_json::from_str::<UrlKey>(&s).unwrap(), k);
}

const SPEC: &str = r#"{
  "id": "fcdo_travel_advice",
  "name": "FCDO foreign travel advice",
  "category": "advisory",
  "source_class": "official",
  "licence": {"class": "open", "spdx": "OGL-UK-3.0", "attribution": "Contains public sector information licensed under the Open Government Licence v3.0.", "url": "https://www.nationalarchives.gov.uk/doc/open-government-licence/version/3/"},
  "method": "api",
  "adapter": "fcdo_content_api",
  "feeds": ["https://www.gov.uk/foreign-travel-advice.atom"],
  "kinds": ["advisory"],
  "cadence": "h6"
}"#;

#[test]
fn source_spec_rejects_unknown_fields() {
    let spec: SourceSpec = serde_json::from_str(SPEC).expect("valid spec parses");
    assert_eq!(spec.max_depth, 2);
    assert_eq!(spec.min_delay_s, 5);
    assert_eq!(spec.daily_budget, 3000);
    assert!(spec.enabled && !spec.needs_key);
    assert_eq!(spec.render, vc_core::sources::RenderMode::Never);

    let mut v: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    v["user_agent"] = serde_json::json!("Mozilla/5.0");
    let err = serde_json::from_value::<SourceSpec>(v)
        .unwrap_err()
        .to_string();
    assert!(err.contains("unknown field"), "{err}");

    let mut v: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    v["licence"]["extra"] = serde_json::json!(1);
    assert!(
        serde_json::from_value::<SourceSpec>(v).is_err(),
        "licence is strict too"
    );

    let mut v: serde_json::Value = serde_json::from_str(SPEC).unwrap();
    v.as_object_mut().unwrap().remove("cadence");
    assert!(
        serde_json::from_value::<SourceSpec>(v).is_err(),
        "cadence is required"
    );
}

#[test]
fn source_spec_enum_spellings() {
    for (field, value) in [
        ("source_class", "commercial_api"),
        ("method", "sitemap"),
        ("cadence", "on_demand"),
        ("render", "auto"),
    ] {
        let mut v: serde_json::Value = serde_json::from_str(SPEC).unwrap();
        v[field] = serde_json::json!(value);
        serde_json::from_value::<SourceSpec>(v).unwrap_or_else(|e| panic!("{field}={value}: {e}"));
    }
    for class in [
        "open",
        "share_alike",
        "facts_only",
        "api_terms",
        "restricted",
    ] {
        let mut v: serde_json::Value = serde_json::from_str(SPEC).unwrap();
        v["licence"]["class"] = serde_json::json!(class);
        serde_json::from_value::<SourceSpec>(v).unwrap();
    }
}
