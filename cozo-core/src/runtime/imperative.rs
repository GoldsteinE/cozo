/*
 * Copyright 2022, The Cozo Project Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;

use either::{Either, Left, Right};
use itertools::Itertools;
use miette::{bail, Diagnostic, Report, Result};
use smartstring::{LazyCompact, SmartString};
use thiserror::Error;

use crate::data::expr::PredicateTypeError;
use crate::data::functions::op_to_bool;
use crate::data::symb::Symbol;
use crate::parse::{ImperativeCondition, ImperativeProgram, ImperativeStmt, SourceSpan};
use crate::runtime::callback::CallbackCollector;
use crate::runtime::transact::SessionTx;
use crate::{DataValue, Db, NamedRows, Poison, Storage, ValidityTs};
use crate::runtime::db::{RunningQueryCleanup, RunningQueryHandle, seconds_since_the_epoch};

enum ControlCode {
    Termination(NamedRows),
    Break(Option<SmartString<LazyCompact>>, SourceSpan),
    Continue(Option<SmartString<LazyCompact>>, SourceSpan),
}

impl<'s, S: Storage<'s>> Db<S> {
    fn execute_imperative_condition(
        &'s self,
        p: &ImperativeCondition,
        tx: &mut SessionTx<'_>,
        cleanups: &mut Vec<(Vec<u8>, Vec<u8>)>,
        cur_vld: ValidityTs,
        span: SourceSpan,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
    ) -> Result<bool> {
        let res = match p {
            Left(rel) => {
                let relation = tx.get_relation(rel, false)?;
                relation.as_named_rows(tx)?
            }
            Right(p) => self.execute_single_program(
                p.clone(),
                tx,
                cleanups,
                cur_vld,
                callback_targets,
                callback_collector,
            )?,
        };
        Ok(match res.rows.first() {
            None => false,
            Some(row) => {
                if row.is_empty() {
                    false
                } else {
                    op_to_bool(&row[row.len() - 1..])?
                        .get_bool()
                        .ok_or_else(|| PredicateTypeError(span, row.last().cloned().unwrap()))?
                }
            }
        })
    }

    fn execute_imperative_stmts(
        &'s self,
        ps: &ImperativeProgram,
        tx: &mut SessionTx<'_>,
        cleanups: &mut Vec<(Vec<u8>, Vec<u8>)>,
        cur_vld: ValidityTs,
        callback_targets: &BTreeSet<SmartString<LazyCompact>>,
        callback_collector: &mut CallbackCollector,
        poison: &Poison
    ) -> Result<Either<NamedRows, ControlCode>> {
        let mut ret = NamedRows::default();
        for p in ps {
            poison.check()?;
            match p {
                ImperativeStmt::Break { target, span, .. } => {
                    return Ok(Right(ControlCode::Break(target.clone(), *span)));
                }
                ImperativeStmt::Continue { target, span, .. } => {
                    return Ok(Right(ControlCode::Continue(target.clone(), *span)));
                }
                ImperativeStmt::Return { returns } => {
                    if returns.is_empty() {
                        return Ok(Right(ControlCode::Termination(NamedRows::default())));
                    }
                    let mut current = None;
                    for nxt in returns.iter().rev() {
                        let mut nr = match nxt {
                            Left(prog) => self.execute_single_program(
                                prog.clone(),
                                tx,
                                cleanups,
                                cur_vld,
                                callback_targets,
                                callback_collector,
                            )?,
                            Right(rel) => {
                                let relation = tx.get_relation(rel, false)?;
                                relation.as_named_rows(tx)?
                            }
                        };
                        nr.next = current;
                        current = Some(Box::new(nr))
                    }
                    return Ok(Right(ControlCode::Termination(*current.unwrap())));
                }
                ImperativeStmt::TempDebug { temp, .. } => {
                    let relation = tx.get_relation(temp, false)?;
                    println!("{}: {:?}", temp, relation.as_named_rows(tx)?);
                    ret = NamedRows::default();
                }
                ImperativeStmt::Program { prog, .. } => {
                    ret = self.execute_single_program(
                        prog.clone(),
                        tx,
                        cleanups,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                    )?;
                }
                ImperativeStmt::IgnoreErrorProgram { prog, .. } => {
                    match self.execute_single_program(
                        prog.clone(),
                        tx,
                        cleanups,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                    ) {
                        Ok(res) => ret = res,
                        Err(_) => {
                            ret = NamedRows::new(
                                vec!["status".to_string()],
                                vec![vec![DataValue::from("FAILED")]],
                            )
                        }
                    }
                }
                ImperativeStmt::If {
                    condition,
                    then_branch,
                    else_branch,
                    span,
                    negated,
                } => {
                    let cond_val = self.execute_imperative_condition(
                        condition,
                        tx,
                        cleanups,
                        cur_vld,
                        *span,
                        callback_targets,
                        callback_collector,
                    )?;
                    let cond_val = if *negated { !cond_val } else { cond_val };
                    let to_execute = if cond_val { then_branch } else { else_branch };
                    match self.execute_imperative_stmts(
                        to_execute,
                        tx,
                        cleanups,
                        cur_vld,
                        callback_targets,
                        callback_collector,
                        poison
                    )? {
                        Left(rows) => {
                            ret = rows;
                        }
                        Right(ctrl) => return Ok(Right(ctrl)),
                    }
                }
                ImperativeStmt::Loop { label, body, .. } => {
                    ret = Default::default();
                    loop {
                        poison.check()?;

                        match self.execute_imperative_stmts(
                            body,
                            tx,
                            cleanups,
                            cur_vld,
                            callback_targets,
                            callback_collector,
                            poison
                        )? {
                            Left(_) => {}
                            Right(ctrl) => match ctrl {
                                ControlCode::Termination(ret) => {
                                    return Ok(Right(ControlCode::Termination(ret)))
                                }
                                ControlCode::Break(break_label, span) => {
                                    if break_label.is_none() || break_label == *label {
                                        break;
                                    } else {
                                        return Ok(Right(ControlCode::Break(break_label, span)));
                                    }
                                }
                                ControlCode::Continue(cont_label, span) => {
                                    if cont_label.is_none() || cont_label == *label {
                                        continue;
                                    } else {
                                        return Ok(Right(ControlCode::Continue(cont_label, span)));
                                    }
                                }
                            },
                        }
                    }
                }
                ImperativeStmt::TempSwap { left, right, .. } => {
                    tx.rename_temp_relation(
                        Symbol::new(left.clone(), Default::default()),
                        Symbol::new(SmartString::from("_*temp*"), Default::default()),
                    )?;
                    tx.rename_temp_relation(
                        Symbol::new(right.clone(), Default::default()),
                        Symbol::new(left.clone(), Default::default()),
                    )?;
                    tx.rename_temp_relation(
                        Symbol::new(SmartString::from("_*temp*"), Default::default()),
                        Symbol::new(right.clone(), Default::default()),
                    )?;
                    ret = NamedRows::default();
                    break;
                }
            }
        }
        Ok(Left(ret))
    }
    pub(crate) fn execute_imperative(
        &'s self,
        cur_vld: ValidityTs,
        ps: &ImperativeProgram,
    ) -> Result<NamedRows, Report> {
        let mut callback_collector = BTreeMap::new();
        let mut write_lock_names = BTreeSet::new();
        for p in ps {
            p.needs_write_locks(&mut write_lock_names);
        }
        let is_write = !write_lock_names.is_empty();
        let write_lock = self.obtain_relation_locks(write_lock_names.iter());
        let _write_lock_guards = write_lock.iter().map(|l| l.read().unwrap()).collect_vec();

        let callback_targets = if is_write {
            self.current_callback_targets()
        } else {
            Default::default()
        };
        let mut cleanups: Vec<(Vec<u8>, Vec<u8>)> = vec![];
        let ret;
        {
            let mut tx = if is_write {
                self.transact_write()?
            } else {
                self.transact()?
            };

            let poison = Poison::default();
            let qid = self.queries_count.fetch_add(1, Ordering::AcqRel);
            let since_the_epoch = seconds_since_the_epoch()?;

            let q_handle = RunningQueryHandle {
                started_at: since_the_epoch,
                poison: poison.clone(),
            };
            self.running_queries.lock().unwrap().insert(qid, q_handle);
            let _guard = RunningQueryCleanup {
                id: qid,
                running_queries: self.running_queries.clone(),
            };

            match self.execute_imperative_stmts(
                &ps,
                &mut tx,
                &mut cleanups,
                cur_vld,
                &callback_targets,
                &mut callback_collector,
                &poison
            )? {
                Left(res) => ret = res,
                Right(ctrl) => match ctrl {
                    ControlCode::Termination(res) => {
                        ret = res;
                    }
                    ControlCode::Break(_, span) | ControlCode::Continue(_, span) => {
                        #[derive(Debug, Error, Diagnostic)]
                        #[error("control flow has nowhere to go")]
                        #[diagnostic(code(eval::dangling_ctrl_flow))]
                        struct DanglingControlFlow(#[label] SourceSpan);

                        bail!(DanglingControlFlow(span))
                    }
                },
            }

            if is_write {
                tx.commit_tx()?;
            } else {
                tx.commit_tx()?;
                assert!(cleanups.is_empty(), "non-empty cleanups on read-only tx");
            }
        }
        #[cfg(not(target_arch = "wasm32"))]
        if !callback_collector.is_empty() {
            self.send_callbacks(callback_collector)
        }

        for (lower, upper) in cleanups {
            self.db.del_range(&lower, &upper)?;
        }
        Ok(ret)
    }
}
