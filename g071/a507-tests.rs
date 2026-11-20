  /// A-507 (g264's kiln runner, whose `PATH` lacked `/sbin`): an unmount is judged by the kernel's mount table, and a
  /// path the table lists no mount at is refused before any call, on every Unix host, with no program run and no `PATH`
  /// read. Do: unmount, and let go of, this crate's own directory, never a mount. Expect both refused `NotMounted`
  /// naming the path. Until A-507 macOS ran `umount` found on the `PATH` and answered its exit status, or the error of
  /// starting it where the `PATH` lacked `/sbin`.
  #[cfg(unix)]
  #[test]
  fn an_unmount_of_a_path_where_nothing_is_mounted_is_refused_not_mounted() {
    let path = env!("CARGO_MANIFEST_DIR");
    let not_mounted = MountFault::NotMounted {
      path: path.to_owned(),
      unreported: Vec::new(),
    };
    assert_eq!(super::unmount(path), Err(not_mounted.clone()));
    assert_eq!(super::unmount_busy(path), Err(not_mounted));
  }

  /// A-507: do build the request a plain unmount and a busy one make; expect the mount point passed whole as the call
  /// takes it, the plain unmount asking no force and the busy one `unmount(2)`'s `MNT_FORCE` (`-z` through
  /// `fusermount3` on Linux), and a path no call can name (a NUL byte in it) refused `MountPoint`, never a panic.
  #[cfg(unix)]
  #[test]
  fn an_unmount_is_asked_as_the_os_takes_it() {
    let plain = super::unmount_request("/Volumes/work", super::Unmounting::Plain).unwrap();
    assert_eq!(plain.point.as_bytes(), b"/Volumes/work");
    assert!(!plain.let_go);
    assert_eq!(super::unmount_flags(&plain) & libc::MNT_FORCE, 0);
    let busy = super::unmount_request("/Volumes/work", super::Unmounting::LetGo).unwrap();
    assert!(busy.let_go);
    assert_eq!(super::unmount_flags(&busy), libc::MNT_FORCE);
    assert_eq!(
      super::unmount_request("/Volumes/a\0b", super::Unmounting::Plain),
      Err(MountFault::MountPoint {
        path: "/Volumes/a\0b".to_owned(),
        errno: None
      })
    );
  }

  /// A-507: do map each error number `unmount(2)` documents; expect `EBUSY` refused `Busy` (a process holds a file on
  /// the mount, `unmount_busy` lets go anyway), `EINVAL` `NotMounted` (not in the mount table), `ENOENT` and `ENOTDIR`
  /// `MountPoint` with the number, and every other number, or none, `Unmount` with it.
  #[cfg(unix)]
  #[test]
  fn the_kernels_unmount_refusals_are_typed() {
    let path = "/Volumes/work";
    assert_eq!(
      super::refusal_of_errno(path, Some(libc::EBUSY)),
      MountFault::Busy {
        path: path.to_owned()
      }
    );
    assert!(
      super::refusal_of_errno(path, Some(libc::EBUSY))
        .to_string()
        .contains("unmount_busy"),
      "a busy refusal names the way to let go"
    );
    assert_eq!(
      super::refusal_of_errno(path, Some(libc::EINVAL)),
      MountFault::NotMounted {
        path: path.to_owned(),
        unreported: Vec::new()
      }
    );
    for errno in [libc::ENOENT, libc::ENOTDIR] {
      assert_eq!(
        super::refusal_of_errno(path, Some(errno)),
        MountFault::MountPoint {
          path: path.to_owned(),
          errno: Some(errno)
        }
      );
    }
    for errno in [Some(libc::EPERM), Some(libc::EIO), None] {
      assert_eq!(
        super::refusal_of_errno(path, errno),
        MountFault::Unmount {
          path: path.to_owned(),
          status: None,
          errno
        }
      );
    }
  }

  /// A-507 (kiln's own rule since g264's 68eda5b14): do judge unmount answers against the table read after them;
  /// expect the mount gone from the table done whatever the call said, the mount still listed the call's refusal or
  /// `StillMounted` when the call said done, and with no table on the platform the call's own answer. The mount an
  /// unmount takes is the last the table lists at its point, a later mount shadowing an earlier one.
  #[cfg(unix)]
  #[test]
  fn an_unmount_is_believed_only_when_the_table_agrees() {
    use slates_bridge_oci::mount_table::{MountEntry, MountIdentity};
    let entry = |mount: u64, point: &str| MountEntry {
      identity: MountIdentity { mount, device: 7 },
      mount_point: point.to_owned(),
      fstype: "nfs".to_owned(),
      source: "slates:/work".to_owned(),
    };
    let path = "/Volumes/work";
    let before = [entry(1, "/"), entry(2, path), entry(3, path)];
    let taken = super::mount_at(&before, path).map(|mounted| mounted.identity);
    assert_eq!(taken, Some(MountIdentity { mount: 3, device: 7 }), "the shadowing mount is taken");
    assert_eq!(super::mount_at(&before, "/Volumes/other"), None);
    let busy = || Err(MountFault::Busy { path: path.to_owned() });
    let gone = [entry(1, "/"), entry(2, path)];
    assert_eq!(super::judged(path, taken, Some(&gone), Ok(())), Ok(()));
    assert_eq!(super::judged(path, taken, Some(&gone), busy()), Ok(()), "the table, not the call");
    assert_eq!(
      super::judged(path, taken, Some(&before), Ok(())),
      Err(MountFault::StillMounted { path: path.to_owned() })
    );
    assert_eq!(super::judged(path, taken, Some(&before), busy()), busy());
    assert_eq!(super::judged(path, None, None, busy()), busy(), "no table: the call's answer");
    assert_eq!(super::judged(path, None, None, Ok(())), Ok(()));
  }
}
