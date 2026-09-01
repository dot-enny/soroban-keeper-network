//! `update_verifier` tests.

use soroban_sdk::{testutils::Address as _, Address};

use super::common::*;
use crate::{KeeperError, TaskStatus};

#[test]
fn test_update_verifier_sets_and_clears_verifier_on_pending_task() {
    let s = setup();
    let owner = s.admin.clone();
    let verifier = Address::generate(&s.env);

    let task_id = register_default_task(&s);

    // Initial task has no verifier
    assert_eq!(s.registry.get_task(&task_id).verifier, None);

    // Set new verifier
    s.registry.update_verifier(&owner, &task_id, &Some(verifier.clone()));
    assert_eq!(s.registry.get_task(&task_id).verifier, Some(verifier));

    // Clear verifier (None)
    s.registry.update_verifier(&owner, &task_id, &None);
    assert_eq!(s.registry.get_task(&task_id).verifier, None);
}

#[test]
fn test_update_verifier_rejects_claimed_task() {
    let s = setup();
    let owner = s.admin.clone();
    let keeper = Address::generate(&s.env);
    let verifier = Address::generate(&s.env);

    let task_id = register_default_task(&s);

    // Keeper claims task
    s.registry.claim_task(&keeper, &task_id);
    assert_eq!(s.registry.get_task(&task_id).status, TaskStatus::Claimed);

    // Owner attempt to update verifier on claimed task must return InvalidTaskStatus
    let result = s.registry.try_update_verifier(&owner, &task_id, &Some(verifier));
    assert_eq!(result, Err(Ok(KeeperError::InvalidTaskStatus)));
}

#[test]
fn test_update_verifier_rejects_non_owner() {
    let s = setup();
    let stranger = Address::generate(&s.env);
    let verifier = Address::generate(&s.env);

    let task_id = register_default_task(&s);

    // Non-owner call must fail with NotTaskOwner
    let result = s.registry.try_update_verifier(&stranger, &task_id, &Some(verifier));
    assert_eq!(result, Err(Ok(KeeperError::NotTaskOwner)));
}
