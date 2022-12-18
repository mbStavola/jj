// Copyright 2021-2022 The Jujutsu Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use itertools::Itertools;

use crate::lock::FileLock;
use crate::op_heads_store::{
    LockedOpHeads, LockedOpHeadsResolver, OpHeadResolutionError, OpHeads, OpHeadsStore,
};
use crate::op_store::{OpStore, OperationId, OperationMetadata};
use crate::operation::Operation;
use crate::{dag_walk, op_store};

pub struct SimpleOpHeadsStore {
    store: Arc<InnerSimpleOpHeadsStore>,
}

impl Debug for SimpleOpHeadsStore {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimpleOpHeadsStore")
            .field("dir", &self.store.dir)
            .finish()
    }
}

/// Manages the very set of current heads of the operation log. This store is
/// simply a directory where each operation id is a file with that name (and no
/// content).
struct InnerSimpleOpHeadsStore {
    dir: PathBuf,
}

struct SimpleOpHeadsStoreLockResolver {
    store: Arc<InnerSimpleOpHeadsStore>,
    _lock: FileLock,
}

impl LockedOpHeadsResolver for SimpleOpHeadsStoreLockResolver {
    fn finish(&self, new_op: &Operation) {
        self.store.add_op_head(new_op.id());
        for old_id in new_op.parent_ids() {
            self.store.remove_op_head(old_id);
        }
    }
}

impl InnerSimpleOpHeadsStore {
    pub fn init(
        dir: &Path,
        op_store: &Arc<dyn OpStore>,
        root_view: &op_store::View,
        operation_metadata: OperationMetadata,
    ) -> (Self, Operation) {
        let root_view_id = op_store.write_view(root_view).unwrap();
        let init_operation = op_store::Operation {
            view_id: root_view_id,
            parents: vec![],
            metadata: operation_metadata,
        };
        let init_operation_id = op_store.write_operation(&init_operation).unwrap();
        let init_operation = Operation::new(op_store.clone(), init_operation_id, init_operation);

        let op_heads_dir = dir.join("simple_op_heads");
        fs::create_dir(&op_heads_dir).unwrap();
        let op_heads_store = InnerSimpleOpHeadsStore { dir: op_heads_dir };
        op_heads_store.add_op_head(init_operation.id());
        (op_heads_store, init_operation)
    }

    pub fn add_op_head(&self, id: &OperationId) {
        std::fs::write(self.dir.join(id.hex()), "").unwrap();
    }

    pub fn remove_op_head(&self, id: &OperationId) {
        // It's fine if the old head was not found. It probably means
        // that we're on a distributed file system where the locking
        // doesn't work. We'll probably end up with two current
        // heads. We'll detect that next time we load the view.
        std::fs::remove_file(self.dir.join(id.hex())).ok();
    }

    pub fn get_op_heads(&self) -> Vec<OperationId> {
        let mut op_heads = vec![];
        for op_head_entry in std::fs::read_dir(&self.dir).unwrap() {
            let op_head_file_name = op_head_entry.unwrap().file_name();
            let op_head_file_name = op_head_file_name.to_str().unwrap();
            if let Ok(op_head) = hex::decode(op_head_file_name) {
                op_heads.push(OperationId::new(op_head));
            }
        }
        op_heads
    }

    /// Removes operations in the input that are ancestors of other operations
    /// in the input. The ancestors are removed both from the list and from
    /// disk.
    /// TODO: Move this into the OpStore trait for sharing
    fn handle_ancestor_ops(&self, op_heads: Vec<Operation>) -> Vec<Operation> {
        let op_head_ids_before: HashSet<_> = op_heads.iter().map(|op| op.id().clone()).collect();
        let neighbors_fn = |op: &Operation| op.parents();
        // Remove ancestors so we don't create merge operation with an operation and its
        // ancestor
        let op_heads = dag_walk::heads(op_heads, &neighbors_fn, &|op: &Operation| op.id().clone());
        let op_head_ids_after: HashSet<_> = op_heads.iter().map(|op| op.id().clone()).collect();
        for removed_op_head in op_head_ids_before.difference(&op_head_ids_after) {
            self.remove_op_head(removed_op_head);
        }
        op_heads.into_iter().collect()
    }
}

impl SimpleOpHeadsStore {
    pub fn init(
        dir: &Path,
        op_store: &Arc<dyn OpStore>,
        root_view: &op_store::View,
        operation_metadata: OperationMetadata,
    ) -> (Self, Operation) {
        let (inner, init_op) =
            InnerSimpleOpHeadsStore::init(dir, op_store, root_view, operation_metadata);
        (
            SimpleOpHeadsStore {
                store: Arc::new(inner),
            },
            init_op,
        )
    }

    pub fn load(dir: &Path) -> Self {
        let op_heads_dir = dir.join("simple_op_heads");

        // TODO: Delete this migration code at 0.8+ or so
        if !op_heads_dir.exists() {
            let old_store = InnerSimpleOpHeadsStore {
                dir: dir.to_path_buf(),
            };
            fs::create_dir(&op_heads_dir).unwrap();
            let new_store = InnerSimpleOpHeadsStore { dir: op_heads_dir };

            for id in old_store.get_op_heads() {
                old_store.remove_op_head(&id);
                new_store.add_op_head(&id);
            }
            return SimpleOpHeadsStore {
                store: Arc::new(new_store),
            };
        }

        SimpleOpHeadsStore {
            store: Arc::new(InnerSimpleOpHeadsStore { dir: op_heads_dir }),
        }
    }
}

impl OpHeadsStore for SimpleOpHeadsStore {
    fn name(&self) -> &str {
        "simple_op_heads_store"
    }

    fn add_op_head(&self, id: &OperationId) {
        self.store.add_op_head(id);
    }

    fn remove_op_head(&self, id: &OperationId) {
        self.store.remove_op_head(id);
    }

    fn get_op_heads(&self) -> Vec<OperationId> {
        self.store.get_op_heads()
    }

    fn lock(&self) -> LockedOpHeads {
        let lock = FileLock::lock(self.store.dir.join("lock"));
        LockedOpHeads::new(Box::new(SimpleOpHeadsStoreLockResolver {
            store: self.store.clone(),
            _lock: lock,
        }))
    }

    fn get_heads(&self, op_store: &Arc<dyn OpStore>) -> Result<OpHeads, OpHeadResolutionError> {
        let mut op_heads = self.get_op_heads();

        if op_heads.is_empty() {
            return Err(OpHeadResolutionError::NoHeads);
        }

        if op_heads.len() == 1 {
            let operation_id = op_heads.pop().unwrap();
            let operation = op_store.read_operation(&operation_id).unwrap();
            return Ok(OpHeads::Single(Operation::new(
                op_store.clone(),
                operation_id,
                operation,
            )));
        }

        // There are multiple heads. We take a lock, then check if there are still
        // multiple heads (it's likely that another process was in the process of
        // deleting on of them). If there are still multiple heads, we attempt to
        // merge all the views into one. We then write that view and a corresponding
        // operation to the op-store.
        // Note that the locking isn't necessary for correctness; we take the lock
        // only to prevent other concurrent processes from doing the same work (and
        // producing another set of divergent heads).
        let locked_op_heads = self.lock();
        let op_head_ids = self.get_op_heads();

        if op_head_ids.is_empty() {
            return Err(OpHeadResolutionError::NoHeads);
        }

        if op_head_ids.len() == 1 {
            let op_head_id = op_head_ids[0].clone();
            let op_head = op_store.read_operation(&op_head_id).unwrap();
            // Return early so we don't write a merge operation with a single parent
            return Ok(OpHeads::Single(Operation::new(
                op_store.clone(),
                op_head_id,
                op_head,
            )));
        }

        let op_heads = op_head_ids
            .iter()
            .map(|op_id: &OperationId| {
                let data = op_store.read_operation(op_id).unwrap();
                Operation::new(op_store.clone(), op_id.clone(), data)
            })
            .collect_vec();
        let mut op_heads = self.store.handle_ancestor_ops(op_heads);

        // Return without creating a merge operation
        if op_heads.len() == 1 {
            return Ok(OpHeads::Single(op_heads.pop().unwrap()));
        }

        op_heads.sort_by_key(|op| op.store_operation().metadata.end_time.timestamp.clone());
        Ok(OpHeads::Unresolved {
            locked_op_heads,
            op_heads,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::fs;
    use std::path::Path;

    use itertools::Itertools;

    use super::InnerSimpleOpHeadsStore;
    use crate::op_heads_store::OpHeadsStore;
    use crate::op_store::OperationId;
    use crate::simple_op_heads_store::SimpleOpHeadsStore;

    fn read_dir(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_str().unwrap().to_string())
            .sorted()
            .collect()
    }

    #[test]
    fn test_simple_op_heads_store_migration() {
        let test_dir = testutils::new_temp_dir();
        let store_path = test_dir.path().join("op_heads");
        fs::create_dir(&store_path).unwrap();

        let op1 = OperationId::from_hex("012345");
        let op2 = OperationId::from_hex("abcdef");
        let mut ops = HashSet::new();
        ops.insert(op1.clone());
        ops.insert(op2.clone());

        let old_store = InnerSimpleOpHeadsStore {
            dir: store_path.clone(),
        };
        old_store.add_op_head(&op1);
        old_store.add_op_head(&op2);

        assert_eq!(vec!["012345", "abcdef"], read_dir(&store_path));
        drop(old_store);

        let new_store = SimpleOpHeadsStore::load(&store_path);
        assert_eq!(&ops, &new_store.get_op_heads().into_iter().collect());
        assert_eq!(vec!["simple_op_heads"], read_dir(&store_path));
        assert_eq!(
            vec!["012345", "abcdef"],
            read_dir(&store_path.join("simple_op_heads"))
        );

        // Migration is idempotent
        let new_store = SimpleOpHeadsStore::load(&store_path);
        assert_eq!(&ops, &new_store.get_op_heads().into_iter().collect());
        assert_eq!(vec!["simple_op_heads"], read_dir(&store_path));
        assert_eq!(
            vec!["012345", "abcdef"],
            read_dir(&store_path.join("simple_op_heads"))
        );
    }
}