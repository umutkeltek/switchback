use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use sb_credentials::provider_accounts::{
    normalize_alias, AccountResolutionQuery, AdjudicationCommand, AliasScheme,
    ProviderAccountAlias, ProviderAccountAuthority, ReconcileRequest, SourcePaths,
};

fn temp_root(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let root = std::env::temp_dir().join(format!("switchback-pa-{name}-{nonce}"));
    fs::create_dir_all(&root).expect("temp root");
    root
}

fn copy_fixture_tree(root: &std::path::Path) -> SourcePaths {
    let fixtures =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/provider-accounts");
    let codex = root.join("home/.codex");
    let authreg = root.join("authreg");
    let multi = codex.join("multi-auth");
    let codexbar = root.join("home/Library/Application Support/CodexBar");
    fs::create_dir_all(&codex).unwrap();
    fs::create_dir_all(&authreg).unwrap();
    fs::create_dir_all(&multi).unwrap();
    fs::create_dir_all(&codexbar).unwrap();
    fs::copy(
        fixtures.join("codex-auth/active.json"),
        codex.join("auth.json"),
    )
    .unwrap();
    for name in ["default.json", "work.json"] {
        fs::copy(
            fixtures.join("switchback-auth-registry").join(name),
            authreg.join(name),
        )
        .unwrap();
    }
    fs::copy(
        fixtures.join("switchback-auth-registry/active.txt"),
        authreg.join(".active"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("switchback-auth-registry/runs.tsv"),
        authreg.join(".runs"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("codex-multi-auth/accounts.json"),
        multi.join("openai-codex-accounts.json"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("codex-multi-auth/quota-cache.json"),
        multi.join("quota-cache.json"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("codexbar/usage-history.jsonl"),
        codexbar.join("usage-history.jsonl"),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    for path in [
        codex.join("auth.json"),
        authreg.join("default.json"),
        authreg.join("work.json"),
        multi.join("openai-codex-accounts.json"),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    SourcePaths {
        codex_auth: Some(codex.join("auth.json")),
        switchback_auth_registry: Some(authreg),
        codex_multi_auth: Some(multi.join("openai-codex-accounts.json")),
        quota_cache: Some(multi.join("quota-cache.json")),
        codexbar_history: Some(codexbar.join("usage-history.jsonl")),
    }
}

#[test]
fn aliases_normalize_with_normative_precedence() {
    let uuid = normalize_alias(ProviderAccountAlias::OpenAiAccountUuid(
        " 01234567-89AB-CDEF-8123-456789ABCDEF ".into(),
    ))
    .unwrap();
    assert_eq!(uuid.value, "01234567-89ab-cdef-8123-456789abcdef");
    assert_eq!(uuid.rank, 1);

    let email = normalize_alias(ProviderAccountAlias::Email(" User@Example.Test ".into())).unwrap();
    assert_ne!(email.value, "user@example.test");
    assert!(email.display.starts_with("u***@"));
    assert_eq!(email.rank, 5);
}

#[test]
fn reconcile_replay_is_idempotent_and_resolves_composite_alias() {
    let root = temp_root("replay");
    let sources = copy_fixture_tree(&root);
    let authority =
        ProviderAccountAuthority::open(root.join("state/provider-accounts.sqlite")).unwrap();
    let first = authority
        .reconcile(ReconcileRequest::apply(sources.clone()).with_now_ms(1_800_000_000_000))
        .unwrap();
    assert!(first.changed);
    assert_eq!(first.revision, 1);
    let first_snapshot = authority.snapshot().unwrap();

    let second = authority
        .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_000_000))
        .unwrap();
    assert!(!second.changed);
    assert_eq!(second.revision, 1);
    assert_eq!(first_snapshot, authority.snapshot().unwrap());

    let resolved = authority
        .resolve(AccountResolutionQuery {
            provider: "openai".into(),
            client: "codex".into(),
            alias_scheme: AliasScheme::CodexBarAccountKey,
            alias_value: "codex:v1:user_fixture:01234567-89ab-cdef-8123-456789abcdef".into(),
            expected_revision: None,
        })
        .unwrap();
    assert_eq!(resolved.credential_pointer.slot(), Some("default"));
}

#[test]
fn dry_run_creates_nothing_and_stale_adjudication_writes_nothing() {
    let root = temp_root("dry-run");
    let sources = copy_fixture_tree(&root);
    let db = root.join("missing/provider-accounts.sqlite");
    let authority = ProviderAccountAuthority::open_read_only(db.clone());
    let dry = authority
        .reconcile(ReconcileRequest::dry_run(sources.clone()).with_now_ms(1_800_000_000_000))
        .unwrap();
    assert!(dry.changed);
    assert_eq!(dry.base_revision, 0);
    assert!(!db.exists());
    assert!(!db.parent().unwrap().exists());

    let authority = ProviderAccountAuthority::open(db).unwrap();
    authority
        .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_000_000))
        .unwrap();
    let before = authority.snapshot().unwrap();
    let err = authority
        .adjudicate(AdjudicationCommand::Merge {
            from: before.accounts[0].id.clone(),
            into: before.accounts[0].id.clone(),
            expected_revision: 0,
        })
        .unwrap_err();
    assert!(err.to_string().contains("stale revision"));
    assert_eq!(before, authority.snapshot().unwrap());
}

#[test]
fn active_identity_and_weak_aliases_follow_precedence() {
    let root = temp_root("precedence");
    let sources = copy_fixture_tree(&root);
    let authority =
        ProviderAccountAuthority::open(root.join("state/provider-accounts.sqlite")).unwrap();
    authority
        .reconcile(ReconcileRequest::apply(sources.clone()).with_now_ms(1_800_000_000_000))
        .unwrap();
    let snapshot = authority.snapshot().unwrap();
    let enrolled: Vec<_> = snapshot
        .accounts
        .iter()
        .filter(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Enrolled)
        .collect();
    assert_eq!(enrolled.len(), 2, "shared org must not merge UUID accounts");
    let active = snapshot.active_clients[0].active_account.as_ref().unwrap();
    let active_account = snapshot.accounts.iter().find(|a| &a.id == active).unwrap();
    assert!(active_account
        .aliases
        .iter()
        .any(|a| a.normalized_value == "01234567-89ab-cdef-8123-456789abcdef"));
    assert!(
        snapshot.accounts.iter().any(|a| a.state
            == sb_credentials::provider_accounts::EnrollmentState::Parked
            && a.aliases
                .iter()
                .any(|b| b.normalized_value == "org-fixture-shared")),
        "org-only inventory evidence remains parked"
    );

    let work = sources
        .switchback_auth_registry
        .as_ref()
        .unwrap()
        .join("work.json");
    let text=fs::read_to_string(&work).unwrap().replace("eyJleHAiOjE5MDAwMDAwMDAsImVtYWlsIjoiYmV0YUBleGFtcGxlLnRlc3QiLCJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGguY2hhdGdwdF9hY2NvdW50X2lkIjoiZmVkY2JhOTgtNzY1NC00MzIxLTgxMjMtZmVkY2JhOTg3NjU0IiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoLm9yZ2FuaXphdGlvbnMiOlt7ImlkIjoib3JnLWZpeHR1cmUtc2hhcmVkIn1dfQ","eyJleHAiOjE5MDAwMDAwMDAsImVtYWlsIjoiYWxwaGFAZXhhbXBsZS50ZXN0IiwiaHR0cHM6Ly9hcGkub3BlbmFpLmNvbS9hdXRoLmNoYXRncHRfYWNjb3VudF9pZCI6ImZlZGNiYTk4LTc2NTQtNDMyMS04MTIzLWZlZGNiYTk4NzY1NCIsImh0dHBzOi8vYXBpLm9wZW5haS5jb20vYXV0aC5vcmdhbml6YXRpb25zIjpbeyJpZCI6Im9yZy1maXh0dXJlLXNoYXJlZCJ9XX0");
    fs::write(&work, text).unwrap();
    authority
        .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_001_000))
        .unwrap();
    let snapshot = authority.snapshot().unwrap();
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .filter(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Enrolled)
            .count(),
        2,
        "shared email must not merge UUID accounts"
    );
}

#[test]
fn failed_import_retains_enrollment_but_clears_fresh_active_state() {
    let root = temp_root("degraded");
    let sources = copy_fixture_tree(&root);
    let authority =
        ProviderAccountAuthority::open(root.join("state/provider-accounts.sqlite")).unwrap();
    authority
        .reconcile(ReconcileRequest::apply(sources.clone()).with_now_ms(1_800_000_000_000))
        .unwrap();
    let enrolled = authority
        .snapshot()
        .unwrap()
        .accounts
        .iter()
        .filter(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Enrolled)
        .count();
    fs::write(sources.codex_auth.as_ref().unwrap(), b"{broken").unwrap();
    let result = authority
        .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_061_000))
        .unwrap();
    assert_eq!(result.revision, 2);
    let snapshot = authority.snapshot().unwrap();
    assert_eq!(
        snapshot
            .accounts
            .iter()
            .filter(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Enrolled)
            .count(),
        enrolled
    );
    assert!(snapshot.active_clients[0].active_account.is_none());
    assert_ne!(
        snapshot.active_clients[0].freshness,
        sb_credentials::provider_accounts::Freshness::Fresh
    );
}

#[test]
fn merge_split_and_capacity_variants_round_trip() {
    use sb_credentials::provider_accounts::{
        CapacityReset, CapacityUsed, CapacityWindow, CapacityWindowKind, ImportSource,
    };
    let windows = vec![
        CapacityWindow {
            window_kind: CapacityWindowKind::Primary,
            window_minutes: Some(300),
            used: CapacityUsed::Percent { used_percent: 10.0 },
            resets_at: CapacityReset::At { resets_at_ms: 123 },
            source: ImportSource::CodexBar,
        },
        CapacityWindow {
            window_kind: CapacityWindowKind::RequestsPerMinute,
            window_minutes: Some(1),
            used: CapacityUsed::Requests {
                used: 2,
                limit: Some(10),
            },
            resets_at: CapacityReset::Rolling,
            source: ImportSource::CodexBar,
        },
        CapacityWindow {
            window_kind: CapacityWindowKind::TokensPerMinute,
            window_minutes: Some(1),
            used: CapacityUsed::Tokens {
                used: 20,
                limit: None,
            },
            resets_at: CapacityReset::Unknown,
            source: ImportSource::CodexBar,
        },
        CapacityWindow {
            window_kind: CapacityWindowKind::ConcurrentSessions,
            window_minutes: None,
            used: CapacityUsed::Concurrent {
                in_use: 1,
                limit: Some(4),
            },
            resets_at: CapacityReset::Unknown,
            source: ImportSource::CodexBar,
        },
    ];
    let encoded = serde_json::to_vec(&windows).unwrap();
    let decoded: Vec<CapacityWindow> = serde_json::from_slice(&encoded).unwrap();
    assert_eq!(windows, decoded);
    let root = temp_root("adjudicate");
    let sources = copy_fixture_tree(&root);
    let authority =
        ProviderAccountAuthority::open(root.join("state/provider-accounts.sqlite")).unwrap();
    authority
        .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_000_000))
        .unwrap();
    let before = authority.snapshot().unwrap();
    let ids: Vec<_> = before
        .accounts
        .iter()
        .filter(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Enrolled)
        .map(|a| a.id.clone())
        .collect();
    let merged = authority
        .adjudicate(AdjudicationCommand::Merge {
            from: ids[1].clone(),
            into: ids[0].clone(),
            expected_revision: 1,
        })
        .unwrap();
    assert_eq!(merged.revision, 2);
    assert!(merged
        .snapshot
        .accounts
        .iter()
        .find(|a| a.id == ids[1])
        .is_some_and(|a| a.state == sb_credentials::provider_accounts::EnrollmentState::Retired));
    let survivor = merged
        .snapshot
        .accounts
        .iter()
        .find(|a| a.id == ids[0])
        .unwrap();
    let binding = survivor
        .aliases
        .iter()
        .find(|b| b.scheme == AliasScheme::OpenAiAccountUuid)
        .map(sb_credentials::provider_accounts::binding_id)
        .unwrap();
    let split = authority
        .adjudicate(AdjudicationCommand::Split {
            account: ids[0].clone(),
            binding,
            expected_revision: 2,
        })
        .unwrap();
    assert_eq!(split.revision, 3);
}

#[test]
fn reconcile_is_atomic_under_race_and_read_operations_do_not_touch_sources() {
    let root = temp_root("race");
    let sources = copy_fixture_tree(&root);
    let db = root.join("state/provider-accounts.sqlite");
    ProviderAccountAuthority::open(&db).unwrap();
    let watched = sources.codex_auth.as_ref().unwrap();
    let before = file_evidence(watched);
    let mut threads = vec![];
    for _ in 0..2 {
        let db = db.clone();
        let sources = sources.clone();
        threads.push(std::thread::spawn(move || {
            ProviderAccountAuthority::open(db)
                .unwrap()
                .reconcile(ReconcileRequest::apply(sources).with_now_ms(1_800_000_000_000))
                .unwrap()
        }));
    }
    let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.changed).count(), 1);
    let authority = ProviderAccountAuthority::open_read_only(&db);
    let snapshot = authority.snapshot().unwrap();
    assert_eq!(snapshot.revision, 1);
    authority
        .reconcile(ReconcileRequest::dry_run(sources.clone()).with_now_ms(1_800_000_000_000))
        .unwrap();
    authority
        .resolve(AccountResolutionQuery {
            provider: "openai".into(),
            client: "codex".into(),
            alias_scheme: AliasScheme::Label,
            alias_value: "default".into(),
            expected_revision: Some(1),
        })
        .unwrap();
    assert_eq!(before, file_evidence(watched));
    let conn = rusqlite::Connection::open(db).unwrap();
    let revisions: i64 = conn
        .query_row("select count(*) from provider_account_revisions", [], |r| {
            r.get(0)
        })
        .unwrap();
    let audits: i64 = conn
        .query_row("select count(*) from provider_account_audit", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!((revisions, audits), (1, 1));
}

fn file_evidence(path: &std::path::Path) -> (u64, u64, u64, u32, String) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let meta = fs::metadata(path).unwrap();
    let bytes = fs::read(path).unwrap();
    let digest = Sha256::digest(bytes);
    (
        meta.ino(),
        meta.len(),
        meta.mtime() as u64,
        meta.permissions().mode(),
        format!("{digest:x}"),
    )
}
