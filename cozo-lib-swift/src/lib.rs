/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use cozo::*;

#[swift_bridge::bridge]
mod ffi {
    extern "Rust" {
        type DbInstance;

        fn new_cozo_db(engine: &str, path: &str, options: &str) -> Option<DbInstance>;

        #[swift_bridge(associated_to = DbInstance)]
        fn run_script_str(&self, payload: &str, params: &str) -> String;
        fn export_relations_str(&self, data: &str) -> String;
        fn import_relations_str(&self, data: &str) -> String;
        fn backup_db_str(&self, out_file: &str) -> String;
        fn restore_backup_str(&self, in_file: &str) -> String;
        fn import_from_backup_str(&self, data: &str) -> String;
    }
}

fn new_cozo_db(engine: &str, path: &str, options: &str) -> Option<DbInstance> {
    let options = if options.is_empty() { "{}" } else { options };
    match DbInstance::new_with_str(engine, path, options) {
        Ok(db) => Some(db),
        Err(err) => {
            eprintln!("{err}");
            None
        }
    }
}
