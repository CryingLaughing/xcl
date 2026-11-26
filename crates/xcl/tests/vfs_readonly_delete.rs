//! Check DeleteFileW through the installed VFS hook in a separate test process.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use windows_sys::Win32::Foundation::{GetLastError, ERROR_ACCESS_DENIED, ERROR_FILE_NOT_FOUND};
use windows_sys::Win32::Storage::FileSystem::{DeleteFileW, MoveFileExW, MOVEFILE_COPY_ALLOWED};
use xcl::util::wide;
use xcl::vfs::Vfs;

const CHILD_ROOT: &str = "XCL_VFS_READONLY_DELETE_TEST_ROOT";
const READONLY_ORIGINAL: &str = "readonly-original.txt";
const READONLY_OVERLAY: &str = "readonly-overlay.txt";
const READONLY_MOVE: &str = "readonly-move.txt";
const WRITABLE: &str = "writable.txt";
const WRITABLE_OVERLAY: &str = "writable-overlay.txt";

fn set_readonly(path: &Path, readonly: bool) -> std::io::Result<()> {
    let mut permissions = fs::metadata(path)?.permissions();
    permissions.set_readonly(readonly);
    fs::set_permissions(path, permissions)
}

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        for path in [
            self.0.join("native-readonly.txt"), self.0.join("game").join(READONLY_ORIGINAL),
            self.0.join("game").join(READONLY_MOVE), self.0.join("moved.txt"),
            data_dir(&self.0).join("overlay").join(READONLY_OVERLAY),
        ] {
            let _ = set_readonly(&path, false);
        }
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn delete(path: &str) -> (bool, u32) {
    let path = wide(path);
    unsafe {
        let deleted = DeleteFileW(path.as_ptr()) != 0;
        (deleted, if deleted { 0 } else { GetLastError() })
    }
}

fn data_dir(root: &Path) -> PathBuf {
    // Keep native overlay paths beyond MAX_PATH while title-visible paths stay short.
    let component = "d".repeat(120);
    root.join("data").join(&component).join(&component)
}

fn make_vfs(root: &Path) -> Vfs {
    let game = root.join("game");
    Vfs::new(&game, &data_dir(root), vec![("GAME:".into(), game.to_string_lossy().into_owned())])
}

fn exercise_hook(root: &Path) {
    let vfs = xcl::vfs::init(make_vfs(root));
    xcl::vfs_hooks::install().unwrap();
    for (name, expected) in [(READONLY_ORIGINAL, b"original".as_slice()), (READONLY_OVERLAY, b"overlay".as_slice())] {
        let path = format!("GAME:\\{name}");
        let (deleted, error) = delete(&path);
        assert!(!deleted, "{name}: virtual DeleteFileW reported success");
        assert_eq!(error, ERROR_ACCESS_DENIED, "{name}: virtual DeleteFileW error");
        let visible = vfs.real_path_for_read(&path).expect("a failed delete leaves the file visible");
        assert_eq!(fs::read(visible).unwrap(), expected);
        assert!(!vfs.is_whiteout(name));
    }
    assert!(!data_dir(root).join("overlay/.xcl_whiteouts").exists());

    let path = format!("GAME:\\{WRITABLE_OVERLAY}");
    assert!(delete(&path).0, "writable overlay file can be deleted through an extended native path");
    assert!(!vfs.exists(WRITABLE_OVERLAY));
    assert!(!vfs.is_whiteout(WRITABLE_OVERLAY));
    assert_eq!(delete(&path), (false, ERROR_FILE_NOT_FOUND));

    let path = format!("GAME:\\{WRITABLE}");
    assert!(delete(&path).0, "writable original can be deleted in the VFS");
    assert!(vfs.real_path_for_read(&path).is_none());
    assert!(vfs.is_whiteout(WRITABLE));
    assert_eq!(delete(&path), (false, ERROR_FILE_NOT_FOUND));

    let path = format!("GAME:\\{READONLY_MOVE}");
    let source = wide(&path);
    let dest = wide(&root.join("moved.txt").to_string_lossy());
    let moved = unsafe { MoveFileExW(source.as_ptr(), dest.as_ptr(), MOVEFILE_COPY_ALLOWED) };
    assert_ne!(moved, 0, "moving a read-only original through the VFS must still succeed");
    assert!(vfs.real_path_for_read(&path).is_none());
}

#[test]
fn delete_file_respects_readonly_attribute() {
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        exercise_hook(Path::new(&root));
        return;
    }
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("xcl_vfs_readonly_delete_{}_{nonce}", std::process::id()));
    fs::create_dir(&dir).unwrap();
    let root = TempDir(dir);
    let game = root.0.join("game");
    let overlay = data_dir(&root.0).join("overlay");
    fs::create_dir(&game).unwrap();
    fs::create_dir_all(&overlay).unwrap();
    fs::write(game.join(READONLY_ORIGINAL), b"original").unwrap();
    fs::write(game.join(READONLY_OVERLAY), b"shadowed original").unwrap();
    fs::write(game.join(READONLY_MOVE), b"readonly move source").unwrap();
    fs::write(overlay.join(READONLY_OVERLAY), b"overlay").unwrap();
    fs::write(overlay.join(WRITABLE_OVERLAY), b"writable overlay").unwrap();
    fs::write(game.join(WRITABLE), b"writable original").unwrap();
    set_readonly(&game.join(READONLY_ORIGINAL), true).unwrap();
    set_readonly(&game.join(READONLY_MOVE), true).unwrap();
    set_readonly(&overlay.join(READONLY_OVERLAY), true).unwrap();

    let native_readonly = root.0.join("native-readonly.txt");
    let native_writable = root.0.join("native-writable.txt");
    fs::write(&native_readonly, b"native readonly").unwrap();
    fs::write(&native_writable, b"native writable").unwrap();
    set_readonly(&native_readonly, true).unwrap();
    assert_eq!(delete(&native_readonly.to_string_lossy()), (false, ERROR_ACCESS_DENIED));
    assert_eq!(fs::read(native_readonly).unwrap(), b"native readonly");
    assert!(delete(&native_writable.to_string_lossy()).0);
    assert!(!native_writable.exists());

    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "delete_file_respects_readonly_attribute", "--nocapture"])
        .env(CHILD_ROOT, &root.0)
        .output()
        .unwrap();
    assert!(output.status.success(), "VFS child exit {}\n{}\n{}", output.status,
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));

    assert_eq!(fs::read(game.join(READONLY_ORIGINAL)).unwrap(), b"original");
    assert_eq!(fs::read(game.join(READONLY_OVERLAY)).unwrap(), b"shadowed original");
    assert_eq!(fs::read(game.join(READONLY_MOVE)).unwrap(), b"readonly move source");
    assert_eq!(fs::read(root.0.join("moved.txt")).unwrap(), b"readonly move source");
    assert_eq!(fs::read(overlay.join(READONLY_OVERLAY)).unwrap(), b"overlay");
    assert_eq!(fs::read(game.join(WRITABLE)).unwrap(), b"writable original");
    assert!(!overlay.join(WRITABLE).exists());
    assert!(!overlay.join(WRITABLE_OVERLAY).exists());
    let reopened = make_vfs(&root.0);
    assert_eq!(reopened.whiteouts(), [READONLY_MOVE.to_string(), WRITABLE.to_string()]);
    assert!(reopened.read_path_rel(READONLY_MOVE).is_none());
    assert!(reopened.read_path_rel(WRITABLE).is_none());
    assert!(reopened.read_path_rel(READONLY_ORIGINAL).is_some());
    assert!(reopened.read_path_rel(READONLY_OVERLAY).is_some());
}
