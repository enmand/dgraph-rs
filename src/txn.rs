use failure::{bail, Error};
use std::collections::HashMap;

use crate::protos::api;
use crate::protos::api_grpc;

pub struct Txn<'a> {
    pub(super) context: api::TxnContext,
    pub(super) finished: bool,
    pub(super) read_only: bool,
    pub(super) best_effort: bool,
    pub(super) mutated: bool,
    pub(super) client: &'a api_grpc::DgraphClient,
}

/// Call Txn::discard() once txn goes out of scope.
/// This is safe to do so, and is possible a no-op
impl Drop for Txn<'_> {
    fn drop(&mut self) {
        let _ = self.discard();
    }
}

impl Txn<'_> {
    /// `best_effort` enables best effort in read-only queries. Using this flag
    /// will ask the Dgraph Alpha to try to get timestamps from memory in a best
    /// effort to reduce the number of outbound requests to Zero. This may yield
    /// improved latencies in read-bound datasets. Returns the transaction itself.
    pub fn best_effort(&mut self) -> Result<&Txn, Error> {
        if !self.read_only {
            bail!("Best effort only works for read-only queries")
        }
        self.best_effort = true;
        Ok(self)
    }

    pub fn query(&mut self, query: impl Into<String>) -> Result<api::Response, Error> {
        self.query_with_vars(query, HashMap::new())
    }

    pub fn query_with_vars(
        &mut self,
        query: impl Into<String>,
        vars: HashMap<String, String>,
    ) -> Result<api::Response, Error> {
        if self.finished {
            bail!("Transaction has already been committed or discarded");
        }

        let res = self.client.query(&api::Request {
            query: query.into(),
            vars,
            read_only: self.read_only,
            best_effort: self.best_effort,
            ..Default::default()
        })?;

        let txn = match res.txn.as_ref() {
            Some(txn) => txn,
            None => bail!("Got empty Txn response back from query"),
        };

        self.merge_context(txn)?;

        Ok(res)
    }

    pub fn mutate(&mut self, mut mu: api::Mutation) -> Result<api::Assigned, Error> {
        match (self.finished, self.read_only) {
            (true, _) => bail!("Txn is finished"),
            (_, true) => bail!("Txn is read only"),
            _ => (),
        }

        self.mutated = true;
        mu.start_ts = self.context.start_ts;
        let commit_now = mu.commit_now;
        let mu_res = self.client.mutate(&mu);

        let mu_res = match mu_res {
            Ok(mu_res) => mu_res,
            Err(e) => {
                let _ = self.discard();
                bail!(e);
            }
        };

        if commit_now {
            self.finished = true;
        }

        {
            let context = match mu_res.context.as_ref() {
                Some(context) => context,
                None => bail!("Missing Txn context on mutation response"),
            };

            self.merge_context(context)?;
        }

        Ok(mu_res)
    }

    pub fn commit(mut self) -> Result<(), Error> {
        match (self.finished, self.read_only) {
            (true, _) => bail!("Txn is finished"),
            (_, true) => bail!("Txn is read only"),
            _ => (),
        }

        self.commit_or_abort()
    }

    pub fn discard(&mut self) -> Result<(), Error> {
        self.context.aborted = true;
        self.commit_or_abort()
    }

    fn commit_or_abort(&mut self) -> Result<(), Error> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;

        if !self.mutated {
            return Ok(());
        }

        self.client.commit_or_abort(&self.context)?;

        Ok(())
    }

    fn merge_context(&mut self, src: &api::TxnContext) -> Result<(), Error> {
        if self.context.start_ts == 0 {
            self.context.start_ts = src.start_ts;
        }

        if self.context.start_ts != src.start_ts {
            bail!("self.context.start_ts != src.start_ts")
        }

        for key in src.keys.iter() {
            self.context.keys.push(key.clone());
        }

        for pred in src.preds.iter() {
            self.context.preds.push(pred.clone());
        }

        Ok(())
    }
}
