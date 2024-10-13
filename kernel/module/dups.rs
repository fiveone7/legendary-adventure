// SPDX-License-Identifier: GPL-2.0-or-later
//
// kmod dups - the kernel module autoloader duplicate suppressor
//
// Copyright (C) 2023 Luis Chamberlain <mcgrof@kernel.org>

use std::sync::{Arc, Mutex};
use std::collections::LinkedList;
use std::thread;
use std::time::Duration;

#[derive(Clone)]
struct KmodDupReq {
    name: String,
    first_req_done: Arc<(Mutex<bool>, std::sync::Condvar)>,
    dup_ret: Arc<Mutex<i32>>,
}

lazy_static::lazy_static! {
    static ref KMOD_DUP_REQS: Mutex<LinkedList<KmodDupReq>> = Mutex::new(LinkedList::new());
}

fn kmod_dup_request_lookup(module_name: &str) -> Option<KmodDupReq> {
    let reqs = KMOD_DUP_REQS.lock().unwrap();
    for req in reqs.iter() {
        if req.name == module_name {
            return Some(req.clone());
        }
    }
    None
}

fn kmod_dup_request_delete(req: KmodDupReq) {
    thread::sleep(Duration::from_secs(60));
    let mut reqs = KMOD_DUP_REQS.lock().unwrap();
    reqs.retain(|r| r.name != req.name);
}

fn kmod_dup_request_complete(req: KmodDupReq) {
    let (lock, cvar) = &*req.first_req_done;
    let mut done = lock.lock().unwrap();
    *done = true;
    cvar.notify_all();
    thread::spawn(move || kmod_dup_request_delete(req));
}

fn kmod_dup_request_exists_wait(module_name: &str, wait: bool, dup_ret: &mut i32) -> bool {
    let new_req = KmodDupReq {
        name: module_name.to_string(),
        first_req_done: Arc::new((Mutex::new(false), std::sync::Condvar::new())),
        dup_ret: Arc::new(Mutex::new(0)),
    };

    let mut reqs = KMOD_DUP_REQS.lock().unwrap();

    if let Some(existing_req) = kmod_dup_request_lookup(module_name) {
        drop(reqs);
        if !wait {
            *dup_ret = 0;
            return true;
        }

        let (lock, cvar) = &*existing_req.first_req_done;
        let mut done = lock.lock().unwrap();
        while !*done {
            done = cvar.wait(done).unwrap();
        }
        *dup_ret = *existing_req.dup_ret.lock().unwrap();
        return true;
    }

    if !wait {
        println!("New request_module_nowait() for {} -- cannot track duplicates for this request", module_name);
        return false;
    }

    println!("New request_module() for {}", module_name);
    reqs.push_back(new_req.clone());
    drop(reqs);
    false
}

fn kmod_dup_request_announce(module_name: &str, ret: i32) {
    let mut reqs = KMOD_DUP_REQS.lock().unwrap();

    if let Some(req) = kmod_dup_request_lookup(module_name) {
        *req.dup_ret.lock().unwrap() = ret;
        drop(reqs);
        thread::spawn(move || kmod_dup_request_complete(req));
    }
}