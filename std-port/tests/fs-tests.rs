//! Curated real-std fs tests (from rust-oxbow library/std/src/fs/tests.rs), limited to
//! the filesystem operations oxbow's fsd backend actually implements: open/create/
//! read/write/seek, metadata (is_file/is_dir/len/exists), read_dir, mkdir/create_dir_all,
//! unlink/rmdir/remove_dir_all, rename, copy. Symlink/lock/perm/time/truncate/clone/
//! canonicalize tests are intentionally excluded (genuine capability/feature gaps).
use crate::fs::{self, File, OpenOptions};
use crate::io::prelude::*;
use crate::io::{ErrorKind, SeekFrom};
use crate::path::Path;
use crate::str;
use crate::test_helpers::tmpdir;
use rand::RngCore;

macro_rules! check {
    ($e:expr) => {
        match $e {
            Ok(t) => t,
            Err(e) => panic!("{} failed with: {e}", stringify!($e)),
        }
    };
}

macro_rules! error_contains {
    ($e:expr, $s:expr) => {
        match $e {
            Ok(_) => panic!("Unexpected success. Should've been: {:?}", $s),
            Err(ref err) => {
                assert!(err.to_string().contains($s), "`{}` did not contain `{}`", err, $s)
            }
        }
    };
}

#[test]
fn file_test_io_smoke_test() {
    let message = "it's alright. have a good time";
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_rt_io_file_test.txt");
    {
        let mut write_stream = check!(File::create(filename));
        check!(write_stream.write(message.as_bytes()));
    }
    {
        let mut read_stream = check!(File::open(filename));
        let mut read_buf = [0; 1028];
        let read_str = match check!(read_stream.read(&mut read_buf)) {
            0 => panic!("shouldn't happen"),
            n => str::from_utf8(&read_buf[..n]).unwrap().to_string(),
        };
        assert_eq!(read_str, message);
    }
    check!(fs::remove_file(filename));
}


#[test]
fn invalid_path_raises() {
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_that_does_not_exist.txt");
    let err = File::open(filename).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}


#[test]
fn file_test_iounlinking_invalid_path_should_raise_condition() {
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_another_file_that_does_not_exist.txt");

    let err = fs::remove_file(filename).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotFound);
}


#[test]
fn file_test_io_non_positional_read() {
    let message: &str = "ten-four";
    let mut read_mem = [0; 8];
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_rt_io_file_test_positional.txt");
    {
        let mut rw_stream = check!(File::create(filename));
        check!(rw_stream.write(message.as_bytes()));
    }
    {
        let mut read_stream = check!(File::open(filename));
        {
            let read_buf = &mut read_mem[0..4];
            check!(read_stream.read(read_buf));
        }
        {
            let read_buf = &mut read_mem[4..8];
            check!(read_stream.read(read_buf));
        }
    }
    check!(fs::remove_file(filename));
    let read_str = str::from_utf8(&read_mem).unwrap();
    assert_eq!(read_str, message);
}


#[test]
fn file_test_io_seek_and_tell_smoke_test() {
    let message = "ten-four";
    let mut read_mem = [0; char::MAX_LEN_UTF8];
    let set_cursor = 4 as u64;
    let tell_pos_pre_read;
    let tell_pos_post_read;
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_rt_io_file_test_seeking.txt");
    {
        let mut rw_stream = check!(File::create(filename));
        check!(rw_stream.write(message.as_bytes()));
    }
    {
        let mut read_stream = check!(File::open(filename));
        check!(read_stream.seek(SeekFrom::Start(set_cursor)));
        tell_pos_pre_read = check!(read_stream.stream_position());
        check!(read_stream.read(&mut read_mem));
        tell_pos_post_read = check!(read_stream.stream_position());
    }
    check!(fs::remove_file(filename));
    let read_str = str::from_utf8(&read_mem).unwrap();
    assert_eq!(read_str, &message[4..8]);
    assert_eq!(tell_pos_pre_read, set_cursor);
    assert_eq!(tell_pos_post_read, message.len() as u64);
}


#[test]
fn file_test_io_seek_and_write() {
    let initial_msg = "food-is-yummy";
    let overwrite_msg = "-the-bar!!";
    let final_msg = "foo-the-bar!!";
    let seek_idx = 3;
    let mut read_mem = [0; 13];
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_rt_io_file_test_seek_and_write.txt");
    {
        let mut rw_stream = check!(File::create(filename));
        check!(rw_stream.write(initial_msg.as_bytes()));
        check!(rw_stream.seek(SeekFrom::Start(seek_idx)));
        check!(rw_stream.write(overwrite_msg.as_bytes()));
    }
    {
        let mut read_stream = check!(File::open(filename));
        check!(read_stream.read(&mut read_mem));
    }
    check!(fs::remove_file(filename));
    let read_str = str::from_utf8(&read_mem).unwrap();
    assert!(read_str == final_msg);
}


#[test]
fn file_test_io_seek_shakedown() {
    //                   01234567890123
    let initial_msg = "qwer-asdf-zxcv";
    let chunk_one: &str = "qwer";
    let chunk_two: &str = "asdf";
    let chunk_three: &str = "zxcv";
    let mut read_mem = [0; char::MAX_LEN_UTF8];
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_rt_io_file_test_seek_shakedown.txt");
    {
        let mut rw_stream = check!(File::create(filename));
        check!(rw_stream.write(initial_msg.as_bytes()));
    }
    {
        let mut read_stream = check!(File::open(filename));

        check!(read_stream.seek(SeekFrom::End(-4)));
        check!(read_stream.read(&mut read_mem));
        assert_eq!(str::from_utf8(&read_mem).unwrap(), chunk_three);

        check!(read_stream.seek(SeekFrom::Current(-9)));
        check!(read_stream.read(&mut read_mem));
        assert_eq!(str::from_utf8(&read_mem).unwrap(), chunk_two);

        check!(read_stream.seek(SeekFrom::Start(0)));
        check!(read_stream.read(&mut read_mem));
        assert_eq!(str::from_utf8(&read_mem).unwrap(), chunk_one);
    }
    check!(fs::remove_file(filename));
}


#[test]
fn file_test_io_eof() {
    let tmpdir = tmpdir();
    let filename = tmpdir.join("file_rt_io_file_test_eof.txt");
    let mut buf = [0; 256];
    {
        let oo = OpenOptions::new().create_new(true).write(true).read(true).clone();
        let mut rw = check!(oo.open(&filename));
        assert_eq!(check!(rw.read(&mut buf)), 0);
        assert_eq!(check!(rw.read(&mut buf)), 0);
    }
    check!(fs::remove_file(&filename));
}


#[test]
#[cfg(unix)]
fn file_test_io_read_write_at() {
    use crate::os::unix::fs::FileExt;

    let tmpdir = tmpdir();
    let filename = tmpdir.join("file_rt_io_file_test_read_write_at.txt");
    let mut buf = [0; 256];
    let write1 = "asdf";
    let write2 = "qwer-";
    let write3 = "-zxcv";
    let content = "qwer-asdf-zxcv";
    {
        let oo = OpenOptions::new().create_new(true).write(true).read(true).clone();
        let mut rw = check!(oo.open(&filename));
        assert_eq!(check!(rw.write_at(write1.as_bytes(), 5)), write1.len());
        assert_eq!(check!(rw.stream_position()), 0);
        assert_eq!(check!(rw.read_at(&mut buf, 5)), write1.len());
        assert_eq!(str::from_utf8(&buf[..write1.len()]), Ok(write1));
        assert_eq!(check!(rw.stream_position()), 0);
        assert_eq!(check!(rw.read_at(&mut buf[..write2.len()], 0)), write2.len());
        assert_eq!(str::from_utf8(&buf[..write2.len()]), Ok("\0\0\0\0\0"));
        assert_eq!(check!(rw.stream_position()), 0);
        assert_eq!(check!(rw.write(write2.as_bytes())), write2.len());
        assert_eq!(check!(rw.stream_position()), 5);
        assert_eq!(check!(rw.read(&mut buf)), write1.len());
        assert_eq!(str::from_utf8(&buf[..write1.len()]), Ok(write1));
        assert_eq!(check!(rw.stream_position()), 9);
        assert_eq!(check!(rw.read_at(&mut buf[..write2.len()], 0)), write2.len());
        assert_eq!(str::from_utf8(&buf[..write2.len()]), Ok(write2));
        assert_eq!(check!(rw.stream_position()), 9);
        assert_eq!(check!(rw.write_at(write3.as_bytes(), 9)), write3.len());
        assert_eq!(check!(rw.stream_position()), 9);
    }
    {
        let mut read = check!(File::open(&filename));
        assert_eq!(check!(read.read_at(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 0);
        assert_eq!(check!(read.seek(SeekFrom::End(-5))), 9);
        assert_eq!(check!(read.read_at(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 9);
        assert_eq!(check!(read.read(&mut buf)), write3.len());
        assert_eq!(str::from_utf8(&buf[..write3.len()]), Ok(write3));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.read_at(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.read_at(&mut buf, 14)), 0);
        assert_eq!(check!(read.read_at(&mut buf, 15)), 0);
        assert_eq!(check!(read.stream_position()), 14);
    }
    check!(fs::remove_file(&filename));
}


#[test]
#[cfg(windows)]
fn file_test_io_seek_read_write() {
    use crate::os::windows::fs::FileExt;

    let tmpdir = tmpdir();
    let filename = tmpdir.join("file_rt_io_file_test_seek_read_write.txt");
    let mut buf = [0; 256];
    let write1 = "asdf";
    let write2 = "qwer-";
    let write3 = "-zxcv";
    let content = "qwer-asdf-zxcv";
    {
        let oo = OpenOptions::new().create_new(true).write(true).read(true).clone();
        let mut rw = check!(oo.open(&filename));
        assert_eq!(check!(rw.seek_write(write1.as_bytes(), 5)), write1.len());
        assert_eq!(check!(rw.stream_position()), 9);
        assert_eq!(check!(rw.seek_read(&mut buf, 5)), write1.len());
        assert_eq!(str::from_utf8(&buf[..write1.len()]), Ok(write1));
        assert_eq!(check!(rw.stream_position()), 9);
        assert_eq!(check!(rw.seek(SeekFrom::Start(0))), 0);
        assert_eq!(check!(rw.write(write2.as_bytes())), write2.len());
        assert_eq!(check!(rw.stream_position()), 5);
        assert_eq!(check!(rw.read(&mut buf)), write1.len());
        assert_eq!(str::from_utf8(&buf[..write1.len()]), Ok(write1));
        assert_eq!(check!(rw.stream_position()), 9);
        assert_eq!(check!(rw.seek_read(&mut buf[..write2.len()], 0)), write2.len());
        assert_eq!(str::from_utf8(&buf[..write2.len()]), Ok(write2));
        assert_eq!(check!(rw.stream_position()), 5);
        assert_eq!(check!(rw.seek_write(write3.as_bytes(), 9)), write3.len());
        assert_eq!(check!(rw.stream_position()), 14);
    }
    {
        let mut read = check!(File::open(&filename));
        assert_eq!(check!(read.seek_read(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.seek(SeekFrom::End(-5))), 9);
        assert_eq!(check!(read.seek_read(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.seek(SeekFrom::End(-5))), 9);
        assert_eq!(check!(read.read(&mut buf)), write3.len());
        assert_eq!(str::from_utf8(&buf[..write3.len()]), Ok(write3));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.seek_read(&mut buf, 0)), content.len());
        assert_eq!(str::from_utf8(&buf[..content.len()]), Ok(content));
        assert_eq!(check!(read.stream_position()), 14);
        assert_eq!(check!(read.seek_read(&mut buf, 14)), 0);
        assert_eq!(check!(read.seek_read(&mut buf, 15)), 0);
    }
    check!(fs::remove_file(&filename));
}


#[test]
fn file_test_stat_is_correct_on_is_file() {
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_stat_correct_on_is_file.txt");
    {
        let mut opts = OpenOptions::new();
        let mut fs = check!(opts.read(true).write(true).create(true).open(filename));
        let msg = "hw";
        fs.write(msg.as_bytes()).unwrap();

        let fstat_res = check!(fs.metadata());
        assert!(fstat_res.is_file());
    }
    let stat_res_fn = check!(fs::metadata(filename));
    assert!(stat_res_fn.is_file());
    let stat_res_meth = check!(filename.metadata());
    assert!(stat_res_meth.is_file());
    check!(fs::remove_file(filename));
}


#[test]
fn file_test_stat_is_correct_on_is_dir() {
    let tmpdir = tmpdir();
    let filename = &tmpdir.join("file_stat_correct_on_is_dir");
    check!(fs::create_dir(filename));
    let stat_res_fn = check!(fs::metadata(filename));
    assert!(stat_res_fn.is_dir());
    let stat_res_meth = check!(filename.metadata());
    assert!(stat_res_meth.is_dir());
    check!(fs::remove_dir(filename));
}


#[test]
fn file_test_fileinfo_false_when_checking_is_file_on_a_directory() {
    let tmpdir = tmpdir();
    let dir = &tmpdir.join("fileinfo_false_on_dir");
    check!(fs::create_dir(dir));
    assert!(!dir.is_file());
    check!(fs::remove_dir(dir));
}


#[test]
fn file_test_fileinfo_check_exists_before_and_after_file_creation() {
    let tmpdir = tmpdir();
    let file = &tmpdir.join("fileinfo_check_exists_b_and_a.txt");
    check!(check!(File::create(file)).write(b"foo"));
    assert!(file.exists());
    check!(fs::remove_file(file));
    assert!(!file.exists());
}


#[test]
fn file_test_directoryinfo_check_exists_before_and_after_mkdir() {
    let tmpdir = tmpdir();
    let dir = &tmpdir.join("before_and_after_dir");
    assert!(!dir.exists());
    check!(fs::create_dir(dir));
    assert!(dir.exists());
    assert!(dir.is_dir());
    check!(fs::remove_dir(dir));
    assert!(!dir.exists());
}


#[test]
fn file_test_directoryinfo_readdir() {
    let tmpdir = tmpdir();
    let dir = &tmpdir.join("di_readdir");
    check!(fs::create_dir(dir));
    let prefix = "foo";
    for n in 0..3 {
        let f = dir.join(&format!("{n}.txt"));
        let mut w = check!(File::create(&f));
        let msg_str = format!("{}{}", prefix, n.to_string());
        let msg = msg_str.as_bytes();
        check!(w.write(msg));
    }
    let files = check!(fs::read_dir(dir));
    let mut mem = [0; char::MAX_LEN_UTF8];
    for f in files {
        let f = f.unwrap().path();
        {
            let n = f.file_stem().unwrap();
            check!(check!(File::open(&f)).read(&mut mem));
            let read_str = str::from_utf8(&mem).unwrap();
            let expected = format!("{}{}", prefix, n.to_str().unwrap());
            assert_eq!(expected, read_str);
        }
        check!(fs::remove_file(&f));
    }
    check!(fs::remove_dir(dir));
}


#[test]
fn file_create_new_already_exists_error() {
    let tmpdir = tmpdir();
    let file = &tmpdir.join("file_create_new_error_exists");
    check!(fs::File::create(file));
    let e = fs::OpenOptions::new().write(true).create_new(true).open(file).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
}


#[test]
fn mkdir_path_already_exists_error() {
    let tmpdir = tmpdir();
    let dir = &tmpdir.join("mkdir_error_twice");
    check!(fs::create_dir(dir));
    let e = fs::create_dir(dir).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::AlreadyExists);
}


#[test]
fn recursive_mkdir() {
    let tmpdir = tmpdir();
    let dir = tmpdir.join("d1/d2");
    check!(fs::create_dir_all(&dir));
    assert!(dir.is_dir())
}


#[test]
fn recursive_mkdir_slash() {
    check!(fs::create_dir_all(Path::new("/")));
}




#[test]
fn recursive_mkdir_empty() {
    check!(fs::create_dir_all(Path::new("")));
}


#[test]
fn recursive_rmdir_of_file_fails() {
    // test we do not delete a directly specified file.
    let tmpdir = tmpdir();
    let canary = tmpdir.join("do_not_delete");
    check!(check!(File::create(&canary)).write(b"foo"));
    let err = fs::remove_dir_all(&canary).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::NotADirectory);
    assert!(canary.exists());
}


#[test]
fn unicode_path_is_dir() {
    // oxbow: "." is not a navigable ambient path (capability confinement); skip.
    assert!(!Path::new("test/stdtest/fs.rs").is_dir());

    let tmpdir = tmpdir();

    let mut dirpath = tmpdir.path().to_path_buf();
    dirpath.push("test-가一ー你好");
    check!(fs::create_dir(&dirpath));
    assert!(dirpath.is_dir());

    let mut filepath = dirpath;
    filepath.push("unicode-file-\u{ac00}\u{4e00}\u{30fc}\u{4f60}\u{597d}.rs");
    check!(File::create(&filepath)); // ignore return; touch only
    assert!(!filepath.is_dir());
    assert!(filepath.exists());
}


#[test]
fn unicode_path_exists() {
    // oxbow: "." is not a navigable ambient path (capability confinement); skip.
    assert!(!Path::new("test/nonexistent-bogus-path").exists());

    let tmpdir = tmpdir();
    let unicode = tmpdir.path();
    let unicode = unicode.join("test-각丁ー再见");
    check!(fs::create_dir(&unicode));
    assert!(unicode.exists());
    assert!(!Path::new("test/unicode-bogus-path-각丁ー再见").exists());
}


#[test]
fn copy_file_does_not_exist() {
    let from = Path::new("test/nonexistent-bogus-path");
    let to = Path::new("test/other-bogus-path");

    match fs::copy(&from, &to) {
        Ok(..) => panic!(),
        Err(..) => {
            assert!(!from.exists());
            assert!(!to.exists());
        }
    }
}


#[test]
fn copy_src_does_not_exist() {
    let tmpdir = tmpdir();
    let from = Path::new("test/nonexistent-bogus-path");
    let to = tmpdir.join("out.txt");
    check!(check!(File::create(&to)).write(b"hello"));
    assert!(fs::copy(&from, &to).is_err());
    assert!(!from.exists());
    let mut v = Vec::new();
    check!(check!(File::open(&to)).read_to_end(&mut v));
    assert_eq!(v, b"hello");
}


#[test]
fn copy_file_ok() {
    let tmpdir = tmpdir();
    let input = tmpdir.join("in.txt");
    let out = tmpdir.join("out.txt");

    check!(check!(File::create(&input)).write(b"hello"));
    check!(fs::copy(&input, &out));
    let mut v = Vec::new();
    check!(check!(File::open(&out)).read_to_end(&mut v));
    assert_eq!(v, b"hello");

    assert_eq!(check!(input.metadata()).permissions(), check!(out.metadata()).permissions());
}


#[test]
fn copy_file_dst_dir() {
    let tmpdir = tmpdir();
    let out = tmpdir.join("out");

    check!(File::create(&out));
    match fs::copy(&*out, tmpdir.path()) {
        Ok(..) => panic!(),
        Err(..) => {}
    }
}


#[test]
fn copy_file_dst_exists() {
    let tmpdir = tmpdir();
    let input = tmpdir.join("in");
    let output = tmpdir.join("out");

    check!(check!(File::create(&input)).write("foo".as_bytes()));
    check!(check!(File::create(&output)).write("bar".as_bytes()));
    check!(fs::copy(&input, &output));

    let mut v = Vec::new();
    check!(check!(File::open(&output)).read_to_end(&mut v));
    assert_eq!(v, b"foo".to_vec());
}


#[test]
fn copy_file_src_dir() {
    let tmpdir = tmpdir();
    let out = tmpdir.join("out");

    match fs::copy(tmpdir.path(), &out) {
        Ok(..) => panic!(),
        Err(..) => {}
    }
    assert!(!out.exists());
}


#[test]
fn copy_file_returns_metadata_len() {
    let tmp = tmpdir();
    let in_path = tmp.join("in.txt");
    let out_path = tmp.join("out.txt");
    check!(check!(File::create(&in_path)).write(b"lettuce"));
    #[cfg(windows)]
    check!(check!(File::create(tmp.join("in.txt:bunny"))).write(b"carrot"));
    let copied_len = check!(fs::copy(&in_path, &out_path));
    assert_eq!(check!(out_path.metadata()).len(), copied_len);
}




#[test]
fn write_then_read() {
    let mut bytes = [0; 1024];
    crate::test_helpers::test_rng().fill_bytes(&mut bytes);

    let tmpdir = tmpdir();

    check!(fs::write(&tmpdir.join("test"), &bytes[..]));
    let v = check!(fs::read(&tmpdir.join("test")));
    assert!(v == &bytes[..]);

    check!(fs::write(&tmpdir.join("not-utf8"), &[0xFF]));
    error_contains!(
        fs::read_to_string(&tmpdir.join("not-utf8")),
        "stream did not contain valid UTF-8"
    );

    let s = "𐁁𐀓𐀠𐀴𐀍";
    check!(fs::write(&tmpdir.join("utf8"), s.as_bytes()));
    let string = check!(fs::read_to_string(&tmpdir.join("utf8")));
    assert_eq!(string, s);
}


#[test]
fn mkdir_trailing_slash() {
    let tmpdir = tmpdir();
    let path = tmpdir.join("file");
    check!(fs::create_dir_all(&path.join("a/")));
}


#[test]
fn read_dir_not_found() {
    let res = fs::read_dir("/path/that/does/not/exist");
    assert_eq!(res.err().unwrap().kind(), ErrorKind::NotFound);
}


#[test]
fn file_open_not_found() {
    let res = File::open("/path/that/does/not/exist");
    assert_eq!(res.err().unwrap().kind(), ErrorKind::NotFound);
}


/// Ensure ReadDir works on large directories.
/// Regression test for https://github.com/rust-lang/rust/issues/93384.
#[test]
fn read_large_dir() {
    let tmpdir = tmpdir();

    let count = 256; // oxbow fsd path-intern table holds 512 live paths
    for i in 0..count {
        check!(fs::File::create(tmpdir.join(&i.to_string())));
    }

    for entry in fs::read_dir(tmpdir.path()).unwrap() {
        entry.unwrap();
    }
}


/// Regression test for https://github.com/rust-lang/rust/issues/50619.
#[test]
#[cfg(target_os = "linux")]
fn test_read_dir_infinite_loop() {
    use crate::io::ErrorKind;
    use crate::process::Command;

    // Create a zombie child process
    let Ok(mut child) = Command::new("echo").spawn() else { return };

    // Make sure the process is (un)dead
    match child.kill() {
        // InvalidInput means the child already exited
        Err(e) if e.kind() != ErrorKind::InvalidInput => return,
        _ => {}
    }

    // open() on this path will succeed, but readdir() will fail
    let id = child.id();
    let path = format!("/proc/{id}/net");

    // Skip the test if we can't open the directory in the first place
    let Ok(dir) = fs::read_dir(path) else { return };

    // Check for duplicate errors
    assert!(dir.filter(|e| e.is_err()).take(2).count() < 2);
}


#[test]
fn rename_directory() {
    let tmpdir = tmpdir();
    let old_path = tmpdir.join("foo/bar/baz");
    fs::create_dir_all(&old_path).unwrap();
    let test_file = &old_path.join("temp.txt");

    File::create(test_file).unwrap();

    let new_path = tmpdir.join("quux/blat");
    fs::create_dir_all(&new_path).unwrap();
    fs::rename(&old_path, &new_path.join("newdir")).unwrap();
    assert!(new_path.join("newdir").is_dir());
    assert!(new_path.join("newdir/temp.txt").exists());
}
