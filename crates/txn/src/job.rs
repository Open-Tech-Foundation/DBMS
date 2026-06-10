//! Generalized write transactions: a [`WriteJob`] runs on the writer thread
//! against a [`WriteCtx`] and may touch any tree reachable from the published
//! root — this is how the catalog layer commits schema + table + index
//! changes atomically (one root install, one fsync pair).

use btree::BTree;
use common::IoBackend;
use pager::{PageId, Pager};

use crate::{Op, Result, TxnError};

/// One write transaction's logic, executed by the single writer thread.
///
/// # Contract: validate, then apply
///
/// A job must perform **all validation before its first mutation** (the same
/// discipline as `DECISIONS.md` D8). Returning an error rejects the whole
/// transaction: the writer restores the root and the freed-page list to their
/// pre-job state, so a contract-abiding job is all-or-nothing. A job that
/// mutates and *then* fails leaks the pages it allocated (they are never
/// published, never read — but never reclaimed either).
///
/// Errors whose [category](common::CategorizedError) is `Io` or `Corruption`
/// are **fatal**: the writer stops and the database becomes unwritable.
pub trait WriteJob<B: IoBackend>: Send + 'static {
    /// The job's result, delivered to the submitter **after** durable commit.
    type Out: Send + 'static;

    /// Run the transaction against the latest state.
    fn apply(self, ctx: &mut WriteCtx<'_, B>) -> Result<Self::Out>;
}

/// The writer-side view a [`WriteJob`] runs against: the evolving published
/// root plus copy-on-write operations over any tree under it.
///
/// Tree mutations return the tree's **new root**; the job threads roots
/// through its edits and finally points the published root (via
/// [`set_root`](Self::set_root)) at a structure that references them.
pub struct WriteCtx<'a, B: IoBackend> {
    pub(crate) pager: &'a Pager<B>,
    pub(crate) root: &'a mut PageId,
    pub(crate) freed: &'a mut Vec<PageId>,
}

impl<B: IoBackend> WriteCtx<'_, B> {
    fn tree(&self) -> BTree<'_, B> {
        BTree::new(self.pager)
    }

    /// The published root as of this point in the transaction.
    pub fn root(&self) -> PageId {
        *self.root
    }

    /// Move the published root. The new root is what a successful commit
    /// installs and readers snapshot.
    pub fn set_root(&mut self, root: PageId) {
        *self.root = root;
    }

    /// Point lookup in any tree.
    pub fn lookup(&self, root: PageId, key: &[u8]) -> Result<Option<Vec<u8>>> {
        Ok(self.tree().lookup(root, key)?)
    }

    /// Collect `[lo, hi)` from any tree in ascending key order.
    pub fn scan(
        &self,
        root: PageId,
        lo: Option<&[u8]>,
        hi: Option<&[u8]>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        Ok(self.tree().range(root, lo, hi)?.collect_all()?)
    }

    /// Insert (or replace) `key` in the tree at `root`, returning the tree's
    /// new root. Superseded pages join the transaction's freed set.
    pub fn insert(&mut self, root: PageId, key: &[u8], value: &[u8]) -> Result<PageId> {
        let edit = self.tree().insert(root, key, value)?;
        self.freed.extend(edit.freed);
        Ok(edit.new_root)
    }

    /// Delete `key` from the tree at `root`, returning the tree's new root.
    pub fn delete(&mut self, root: PageId, key: &[u8]) -> Result<PageId> {
        let edit = self.tree().delete(root, key)?;
        self.freed.extend(edit.freed);
        Ok(edit.new_root)
    }

    /// Create a fresh empty tree, returning its root.
    pub fn create_tree(&mut self) -> Result<PageId> {
        Ok(self.tree().create()?)
    }

    /// Retire a whole tree: every page reachable from `root` joins the freed
    /// set (reclaimed once no live snapshot can see it).
    pub fn free_tree(&mut self, root: PageId) -> Result<()> {
        let pages = self.tree().pages(root)?;
        self.freed.extend(pages);
        Ok(())
    }
}

/// The plain key/value job: a list of [`Op`]s applied to the tree at the
/// published root. This is the default job type of [`Db`](crate::Db).
pub struct OpsJob(pub Vec<Op>);

impl<B: IoBackend> WriteJob<B> for OpsJob {
    type Out = ();

    fn apply(self, ctx: &mut WriteCtx<'_, B>) -> Result<()> {
        // Validate every op before any mutation (D8).
        for op in &self.0 {
            if let Op::Put(key, value) = op {
                btree::check_entry(key, value).map_err(TxnError::BTree)?;
            }
        }
        let mut root = ctx.root();
        for op in self.0 {
            root = match op {
                Op::Put(key, value) => ctx.insert(root, &key, &value)?,
                Op::Delete(key) => ctx.delete(root, &key)?,
            };
        }
        ctx.set_root(root);
        Ok(())
    }
}
