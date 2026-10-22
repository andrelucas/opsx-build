use super::*;
use crate::{backend::StageSignal, usage::tests::RecordingUi};

struct Repo(std::path::PathBuf);
impl Repo {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("opsx-review-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&path)
                .status()
                .unwrap()
                .success()
        );
        Self(path)
    }
}
impl Drop for Repo {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn finding(id: &str, status: FindingStatus) -> Finding {
    Finding {
        id: id.into(),
        kind: FindingKind::Assertion,
        origin: FindingOrigin::PreviouslyMissed,
        evidence_kind: EvidenceKind::Reproduced,
        status,
        sources: vec!["contract.md#REQ-01".into()],
        scope: vec!["public deadline boundary".into()],
        consequence: "Required refusal is unobserved".into(),
        evidence: "Removing the guard leaves the required case passing".into(),
        repair: "Test the required refusal and relevant boundary alternatives".into(),
    }
}
fn result(findings: Vec<Finding>, signal: StageSignal) -> StageResult {
    StageResult {
        text: "Checks ran with real outcomes; live premises remain a limitation".into(),
        session_id: Some("review-session".into()),
        signal,
        review: Some(Report {
            findings,
            areas: vec![Area {
                id: "deadline".into(),
                sources: vec!["contract.md#REQ-01".into()],
                paths: vec!["shared.go".into()],
                status: AreaStatus::Reviewed,
                limitations: "No native qualification claimed".into(),
            }],
        }),
    }
}

#[test]
fn repair_claims_do_not_close_findings_and_later_evidence_can_reopen_them() {
    let repo = Repo::new();
    let ui = RecordingUi::default();
    let mut history = History::default();
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(vec![finding("", FindingStatus::Open)], StageSignal::Retry),
            vec![],
            &ui,
        )
        .unwrap();
    assert!(history.findings.contains_key("F0001"));
    history.record_work(
        "Repair",
        &StageResult {
            text: "All defects fixed".into(),
            session_id: None,
            signal: StageSignal::Ready,
            review: None,
        },
    );
    assert!(history.has_open_findings());
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(
                vec![finding("F0001", FindingStatus::Closed)],
                StageSignal::Verified,
            ),
            vec![],
            &ui,
        )
        .unwrap();
    assert!(!history.has_open_findings());
    let mut history: History =
        serde_json::from_slice(&serde_json::to_vec(&history).unwrap()).unwrap();
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(
                vec![finding("F0001", FindingStatus::Open)],
                StageSignal::Retry,
            ),
            vec![],
            &ui,
        )
        .unwrap();
    assert_eq!(history.findings["F0001"].origin, FindingOrigin::Reopened);
    assert_eq!(
        history.rounds[0].report.as_ref().unwrap().findings[0].status,
        FindingStatus::Open
    );
    assert_eq!(
        history.rounds[1].report.as_ref().unwrap().findings[0].status,
        FindingStatus::Closed
    );
    assert_eq!(
        fs::read_dir(repo.0.join(".git/opsx-build/reviews"))
            .unwrap()
            .count(),
        3
    );
}

#[test]
fn omitted_blocker_and_unknown_closure_cannot_erase_review_history() {
    let repo = Repo::new();
    let ui = RecordingUi::default();
    let mut history = History::default();
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(vec![finding("", FindingStatus::Open)], StageSignal::Retry),
            vec![],
            &ui,
        )
        .unwrap();
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(vec![], StageSignal::Verified),
            vec![],
            &ui,
        )
        .unwrap();
    assert!(history.has_open_findings());
    let mut unsupported = finding("F0001", FindingStatus::Closed);
    unsupported.evidence.clear();
    assert!(
        history
            .record(
                &repo.0,
                "test",
                "fixture",
                &result(vec![unsupported], StageSignal::Verified),
                vec![],
                &ui
            )
            .is_err()
    );
    assert!(
        history
            .record(
                &repo.0,
                "test",
                "fixture",
                &result(
                    vec![finding("F9999", FindingStatus::Closed)],
                    StageSignal::Verified
                ),
                vec![],
                &ui
            )
            .is_err()
    );
    assert_eq!(history.rounds.len(), 2);
    assert!(history.has_open_findings());
}

#[test]
fn omitted_partial_review_area_does_not_become_complete() {
    let repo = Repo::new();
    let ui = RecordingUi::default();
    let mut history = History::default();
    let mut pending = result(vec![], StageSignal::Incomplete);
    pending.review.as_mut().unwrap().areas[0].status = AreaStatus::Partial;
    history
        .record(&repo.0, "test", "fixture", &pending, vec![], &ui)
        .unwrap();
    let mut later = result(vec![], StageSignal::Verified);
    later.review.as_mut().unwrap().areas[0].id = "another-area".into();
    history
        .record(&repo.0, "test", "fixture", &later, vec![], &ui)
        .unwrap();
    assert!(history.has_unfinished_areas());
    assert_eq!(history.areas["deadline"].status, AreaStatus::Partial);
}

#[test]
fn scope_changes_archive_the_old_candidate_evidence_before_starting_fresh() {
    let repo = Repo::new();
    let ui = RecordingUi::default();
    let mut history = History::default();
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(vec![finding("", FindingStatus::Open)], StageSignal::Retry),
            vec![],
            &ui,
        )
        .unwrap();
    history.reset_for_scope_change(&repo.0, &ui).unwrap();
    assert!(!history.has_open_findings());
    assert!(history.rounds.is_empty());
    let archive = fs::read_dir(repo.0.join(".git/opsx-build/reviews"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.to_string_lossy().ends_with("-superseded.json"))
        .unwrap();
    let prior: History = serde_json::from_slice(&fs::read(archive).unwrap()).unwrap();
    assert!(prior.has_open_findings());
    assert_eq!(prior.rounds.len(), 1);
}

#[test]
fn incomplete_or_inconsistent_reports_request_continuation_instead_of_false_success() {
    let mut pending = result(vec![], StageSignal::Verified);
    pending.review.as_mut().unwrap().areas[0].status = AreaStatus::Partial;
    assert_eq!(pending.normalize_review().signal, StageSignal::Incomplete);
    assert_eq!(
        result(
            vec![finding("", FindingStatus::Open)],
            StageSignal::Verified
        )
        .normalize_review()
        .signal,
        StageSignal::Incomplete
    );
    assert_eq!(
        result(
            vec![finding("", FindingStatus::Observation)],
            StageSignal::Retry
        )
        .normalize_review()
        .signal,
        StageSignal::Incomplete
    );
    assert_eq!(
        result(
            vec![finding("", FindingStatus::Observation)],
            StageSignal::Verified
        )
        .normalize_review()
        .signal,
        StageSignal::Verified
    );
}

#[test]
fn handoff_identifies_shared_changes_and_keeps_observations_out_of_repair_work() {
    let repo = Repo::new();
    let ui = RecordingUi::default();
    let mut history = History::default();
    let before = RepositoryEvidence {
        root: repo.0.clone(),
        files: Some(BTreeMap::from([(
            "shared.go".into(),
            ("old".into(), false),
        )])),
    };
    history
        .record(
            &repo.0,
            "test",
            "fixture",
            &result(
                vec![
                    finding("", FindingStatus::Open),
                    finding("", FindingStatus::Observation),
                ],
                StageSignal::Retry,
            ),
            vec![before],
            &ui,
        )
        .unwrap();
    let current = RepositoryEvidence {
        root: repo.0.clone(),
        files: Some(BTreeMap::from([(
            "shared.go".into(),
            ("new".into(), false),
        )])),
    };
    let handoff = history.handoff(&[current], true).unwrap();
    assert!(handoff.contains("shared.go"));
    assert!(handoff.contains("prior attestations"));
    assert!(handoff.contains("live premises"));
    let repair = history.repair_findings().unwrap();
    assert!(repair.contains("F0001"));
    assert!(!repair.contains("F0002"));
    assert!(history.has_open_findings());
}
