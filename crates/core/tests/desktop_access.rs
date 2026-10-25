use farflow_core::{Command, Error, GitWorkspace, Id, LocalWorkspace, Member, Request, Role};
use std::collections::BTreeSet;

fn member(account: &str, active: bool) -> Member {
    Member {
        id: Id::new_v4(),
        account: account.into(),
        name: account.into(),
        department: "研发".into(),
        roles: BTreeSet::from([Role::Owner]),
        active,
    }
}

fn add(actor: Id, member: Member) -> Request {
    Request {
        id: Id::new_v4(),
        actor_id: actor,
        at: 1792900000,
        command: Command::AddMember { member },
    }
}

#[test]
fn local_identity_selection_and_revision_check_use_fresh_transaction_state() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("本地");
    let owner = member("owner", true);
    let mut first =
        LocalWorkspace::create(&path, Id::new_v4(), "本地工作空间".into(), owner.clone()).unwrap();
    assert_eq!(first.identities().unwrap().len(), 1);
    let mut second = LocalWorkspace::open(&path).unwrap();
    let inactive = member("disabled", false);
    let request = add(owner.id, inactive.clone());
    let receipt = first.execute_checked(request.clone(), 0).unwrap();
    assert_eq!(receipt.revision, 1);
    assert_eq!(
        first
            .execute_checked(request.clone(), 0)
            .unwrap()
            .operation_id,
        request.id
    );
    let choices = second.identities().unwrap();
    assert_eq!(choices.len(), 2);
    assert!(!choices.iter().find(|m| m.id == inactive.id).unwrap().active);
    assert!(matches!(
        second.snapshot(inactive.id),
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        second.execute_checked(add(owner.id, member("stale", true)), 0),
        Err(Error::Conflict(_))
    ));
    assert_eq!(second.identities().unwrap().len(), 2);
    assert!(matches!(
        second.execute_checked(add(inactive.id, member("denied", true)), 0),
        Err(Error::Forbidden(_))
    ));
    let mut reused = request;
    reused.command = Command::AddMember {
        member: member("changed", true),
    };
    assert!(matches!(
        second.execute_checked(reused, 0),
        Err(Error::Conflict(_))
    ));
    second
        .execute_checked(add(owner.id, member("fresh", true)), 1)
        .unwrap();
    drop(first);
    drop(second);
    assert_eq!(
        LocalWorkspace::open(path)
            .unwrap()
            .identities()
            .unwrap()
            .len(),
        3
    );
}

#[test]
fn git_identity_selection_and_revision_check_share_the_repository_lock() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("Git");
    let owner = member("owner", true);
    let first =
        GitWorkspace::create(&path, Id::new_v4(), "Git工作空间".into(), owner.clone()).unwrap();
    let second = GitWorkspace::open(&path).unwrap();
    assert_eq!(first.identities().unwrap().len(), 1);
    let inactive = member("disabled", false);
    let request = add(owner.id, inactive.clone());
    assert_eq!(
        first
            .execute_checked(request.clone(), 0)
            .unwrap()
            .receipt
            .revision,
        1
    );
    assert_eq!(
        second
            .execute_checked(request.clone(), 0)
            .unwrap()
            .receipt
            .operation_id,
        request.id
    );
    assert_eq!(second.identities().unwrap().len(), 2);
    assert!(
        !second
            .identities()
            .unwrap()
            .iter()
            .find(|m| m.id == inactive.id)
            .unwrap()
            .active
    );
    assert!(matches!(
        second.snapshot(inactive.id),
        Err(Error::Forbidden(_))
    ));
    assert!(matches!(
        second.execute_checked(add(owner.id, member("stale", true)), 0),
        Err(Error::Conflict(_))
    ));
    assert!(matches!(
        second.execute_checked(add(inactive.id, member("denied", true)), 0),
        Err(Error::Forbidden(_))
    ));
    let mut reused = request;
    reused.command = Command::AddMember {
        member: member("changed", true),
    };
    assert!(matches!(
        second.execute_checked(reused, 0),
        Err(Error::Conflict(_))
    ));
    second
        .execute_checked(add(owner.id, member("fresh", true)), 1)
        .unwrap();
    assert_eq!(
        GitWorkspace::open(path)
            .unwrap()
            .identities()
            .unwrap()
            .len(),
        3
    );
}
