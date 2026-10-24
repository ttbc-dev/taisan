//! `handle_command` failure semantics: a genuine internal store fault
//! propagates to the caller instead of hardening into a durable
//! `Ineligible` rejection under the caller's idempotency key, while real
//! constraint conflicts still retry and reject.

use std::cell::Cell;

use workloop_core::command::*;
use workloop_core::domain::*;
use workloop_core::store::{Effect, ReadView, Store, StoreError, TxView};
use workloop_core::work::{WorkloopConfig, handle_command};

const NOW: i64 = 1_700_000_000;

fn config() -> WorkloopConfig {
    WorkloopConfig {
        default_repository: "local/repo".into(),
        allowed_effects: AllowedEffects {
            workspace_write: true,
            github_write: false,
            network: false,
        },
        budget: ExecutionBudget::default(),
    }
}

fn submit(key: &str) -> CommandRequest {
    CommandRequest {
        actor: Actor {
            kind: ActorKind::Human,
            external_id: "op".into(),
        },
        origin: Origin("cli".into()),
        idempotency_key: key.into(),
        body: CommandBody::Submit {
            intent: "ship it".into(),
            issue_refs: vec![],
            constraints: vec![],
            verification_argv: vec![],
        },
    }
}

/// A store whose `apply` always fails with `fail`; the few reads a
/// Submit decision performs are answered minimally so the failure lands
/// in the decide+apply attempt loop, exactly where a real store fault
/// would surface.
struct FailingApplyStore {
    fail: StoreError,
    saved: Cell<usize>,
}

struct FailingTx<'a> {
    fail: &'a StoreError,
    saved: &'a Cell<usize>,
}

impl ReadView for FailingTx<'_> {
    fn stored_decision(
        &self,
        _origin: &Origin,
        _actor: &Actor,
        _idempotency_key: &str,
    ) -> Result<Option<(String, CommandDecision)>, StoreError> {
        Ok(None)
    }
    fn command_decision(
        &self,
        _command_id: &CommandId,
    ) -> Result<Option<CommandDecision>, StoreError> {
        Ok(None)
    }
    fn work(&self, _work_id: &WorkId) -> Result<Option<Work>, StoreError> {
        Ok(None)
    }
    fn execution(&self, _execution_id: &ExecutionId) -> Result<Option<Execution>, StoreError> {
        unimplemented!()
    }
    fn active_execution(&self, _work_id: &WorkId) -> Result<Option<Execution>, StoreError> {
        unimplemented!()
    }
    fn executions_for_work(&self, _work_id: &WorkId) -> Result<Vec<Execution>, StoreError> {
        unimplemented!()
    }
    fn queued_executions(&self) -> Result<Vec<Execution>, StoreError> {
        unimplemented!()
    }
    fn reservation(&self, _resource_key: &str) -> Result<Option<ResourceReservation>, StoreError> {
        unimplemented!()
    }
    fn reservations(&self) -> Result<Vec<ResourceReservation>, StoreError> {
        unimplemented!()
    }
    fn slot_limit(&self) -> Result<u32, StoreError> {
        unimplemented!()
    }
    fn slot_holders(&self) -> Result<Vec<(ExecutionId, EventSequence)>, StoreError> {
        unimplemented!()
    }
    fn pending_requests(&self) -> Result<Vec<InvocationRequest>, StoreError> {
        unimplemented!()
    }
    fn request(&self, _request_id: &str) -> Result<Option<InvocationRequest>, StoreError> {
        unimplemented!()
    }
    fn invocations_for_execution(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Vec<Invocation>, StoreError> {
        unimplemented!()
    }
    fn invocation_for_request(&self, _request_id: &str) -> Result<Option<Invocation>, StoreError> {
        unimplemented!()
    }
    fn unresolved_invocations(&self) -> Result<Vec<Invocation>, StoreError> {
        unimplemented!()
    }
    fn works(&self) -> Result<Vec<Work>, StoreError> {
        unimplemented!()
    }
    fn nonterminal_executions(&self) -> Result<Vec<Execution>, StoreError> {
        unimplemented!()
    }
    fn stop_intent(&self, _invocation_id: &InvocationId) -> Result<Option<StopIntent>, StoreError> {
        unimplemented!()
    }
    fn deferrals(&self) -> Result<Vec<workloop_core::store::Deferral>, StoreError> {
        unimplemented!()
    }
    fn attention_acks(&self, _work_id: &WorkId) -> Result<Vec<String>, StoreError> {
        unimplemented!()
    }
    fn observations_for_execution(
        &self,
        _execution_id: &ExecutionId,
    ) -> Result<Vec<workloop_core::observation::Observation>, StoreError> {
        unimplemented!()
    }
    fn observations_for_work(
        &self,
        _work_id: &WorkId,
    ) -> Result<Vec<workloop_core::observation::Observation>, StoreError> {
        unimplemented!()
    }
    fn action_intents(&self) -> Result<Vec<workloop_core::store::PrActionIntent>, StoreError> {
        unimplemented!()
    }
    fn judgments(
        &self,
        _work_id: &WorkId,
    ) -> Result<Vec<(String, Vec<FindingDecision>)>, StoreError> {
        unimplemented!()
    }
    fn last_seq(&self) -> Result<EventSequence, StoreError> {
        Ok(0)
    }
}

impl TxView for FailingTx<'_> {
    fn alloc_seq(&mut self) -> Result<EventSequence, StoreError> {
        Ok(1)
    }
    fn alloc_id(&mut self, prefix: &str) -> Result<String, StoreError> {
        Ok(format!("{prefix}-1"))
    }
    fn apply(&mut self, _effect: &Effect) -> Result<(), StoreError> {
        Err(self.fail.clone())
    }
    fn begin_attempt(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
    fn revert_attempt(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
    fn commit_attempt(&mut self) -> Result<(), StoreError> {
        Ok(())
    }
    fn save_decision(
        &mut self,
        _origin: &Origin,
        _actor: &Actor,
        _idempotency_key: &str,
        _payload_digest: &str,
        _decision: &CommandDecision,
    ) -> Result<(), StoreError> {
        self.saved.set(self.saved.get() + 1);
        Ok(())
    }
}

impl Store for FailingApplyStore {
    fn transact<R>(
        &self,
        f: impl FnOnce(&mut dyn TxView) -> Result<R, StoreError>,
    ) -> Result<R, StoreError> {
        let mut tx = FailingTx {
            fail: &self.fail,
            saved: &self.saved,
        };
        f(&mut tx)
    }
}

#[test]
fn internal_apply_failure_reaches_the_caller_unpersisted() {
    let store = FailingApplyStore {
        fail: StoreError::Internal {
            detail: "injected disk fault".into(),
        },
        saved: Cell::new(0),
    };
    let result = handle_command(&store, &config(), &submit("k-internal"), NOW);
    match result {
        Err(StoreError::Internal { detail }) => {
            assert!(detail.contains("injected disk fault"));
        }
        other => panic!("internal fault must propagate, got {other:?}"),
    }
    assert_eq!(
        store.saved.get(),
        0,
        "no decision may be persisted under the caller's idempotency key"
    );
}

#[test]
fn constraint_conflict_still_retries_then_persists_ineligible() {
    let store = FailingApplyStore {
        fail: StoreError::ConstraintConflict {
            detail: "injected uniqueness conflict".into(),
        },
        saved: Cell::new(0),
    };
    let decision = handle_command(&store, &config(), &submit("k-conflict"), NOW).unwrap();
    assert!(
        matches!(
            decision,
            CommandDecision::Rejected {
                reason: RejectionCode::Ineligible,
                ..
            }
        ),
        "a real conflict degrades to a durable rejection: {decision:?}"
    );
    assert_eq!(store.saved.get(), 1, "the rejection is recorded once");
}
