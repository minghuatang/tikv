use crate::{SkiplistEngine, SkiplistWriteBatch};

use engine_traits::{
    Iterable, MiscExt, Mutable, Peekable, SyncMutable, WriteBatch, WriteBatchExt, WriteOptions,
    CF_DEFAULT, MAX_DELETE_BATCH_SIZE,
};
use kvproto::raft_serverpb::RaftLocalState;
use protobuf::Message;
use raft::{eraftpb::Entry, StorageError};
use raft_engine::{CacheStats, Error, RaftEngine, RaftLogBatch, Result};

const RAFT_LOG_MULTI_GET_CNT: u64 = 8;

impl RaftEngine for SkiplistEngine {
    type LogBatch = SkiplistWriteBatch;

    fn log_batch(&self, capacity: usize) -> Self::LogBatch {
        SkiplistWriteBatch::with_capacity(self, capacity)
    }

    fn sync(&self) -> Result<()> {
        box_try!(self.sync_wal());
        Ok(())
    }

    fn get_raft_state(&self, raft_group_id: u64) -> Result<Option<RaftLocalState>> {
        let key = keys::raft_state_key(raft_group_id);
        let state = box_try!(self.get_msg_cf(CF_DEFAULT, &key));
        Ok(state)
    }

    fn get_entry(&self, raft_group_id: u64, index: u64) -> Result<Option<Entry>> {
        let key = keys::raft_log_key(raft_group_id, index);
        let entry = box_try!(self.get_msg_cf(CF_DEFAULT, &key));
        Ok(entry)
    }

    fn fetch_entries_to(
        &self,
        region_id: u64,
        low: u64,
        high: u64,
        max_size: Option<usize>,
        buf: &mut Vec<Entry>,
    ) -> Result<usize> {
        let (max_size, mut total_size, mut count) = (max_size.unwrap_or(usize::MAX), 0, 0);

        if high - low <= RAFT_LOG_MULTI_GET_CNT {
            // If election happens in inactive regions, they will just try to fetch one empty log.
            for i in low..high {
                if total_size > 0 && total_size >= max_size {
                    break;
                }
                let key = keys::raft_log_key(region_id, i);
                match self.get_value(&key) {
                    Ok(None) => return Err(Error::Storage(StorageError::Unavailable)),
                    Ok(Some(v)) => {
                        let mut entry = Entry::default();
                        entry.merge_from_bytes(&v)?;
                        assert_eq!(entry.get_index(), i);
                        buf.push(entry);
                        total_size += v.len();
                        count += 1;
                    }
                    Err(e) => return Err(box_err!(e)),
                }
            }
            return Ok(count);
        }

        let (mut check_compacted, mut next_index) = (true, low);
        let start_key = keys::raft_log_key(region_id, low);
        let end_key = keys::raft_log_key(region_id, high);
        box_try!(self.scan(
            &start_key,
            &end_key,
            true, // fill_cache
            |_, value| {
                let mut entry = Entry::default();
                entry.merge_from_bytes(value)?;

                if check_compacted {
                    if entry.get_index() != low {
                        // May meet gap or has been compacted.
                        return Ok(false);
                    }
                    check_compacted = false;
                } else {
                    assert_eq!(entry.get_index(), next_index);
                }
                next_index += 1;

                buf.push(entry);
                total_size += value.len();
                count += 1;
                Ok(total_size < max_size)
            },
        ));

        // If we get the correct number of entries, returns.
        // Or the total size almost exceeds max_size, returns.
        if count == (high - low) as usize || total_size >= max_size {
            return Ok(count);
        }

        // Here means we don't fetch enough entries.
        Err(Error::Storage(StorageError::Unavailable))
    }

    fn consume(&self, batch: &mut Self::LogBatch, sync_log: bool) -> Result<usize> {
        let bytes = batch.data_size();
        let mut opts = WriteOptions::default();
        opts.set_sync(sync_log);
        box_try!(self.write_opt(batch, &opts));
        batch.clear();
        Ok(bytes)
    }

    fn consume_and_shrink(
        &self,
        batch: &mut Self::LogBatch,
        sync_log: bool,
        max_capacity: usize,
        shrink_to: usize,
    ) -> Result<usize> {
        let data_size = self.consume(batch, sync_log)?;
        if data_size > max_capacity {
            *batch = self.write_batch_with_cap(shrink_to);
        }
        Ok(data_size)
    }

    fn clean(
        &self,
        raft_group_id: u64,
        state: &RaftLocalState,
        batch: &mut SkiplistWriteBatch,
    ) -> Result<()> {
        box_try!(batch.delete(&keys::raft_state_key(raft_group_id)));
        let seek_key = keys::raft_log_key(raft_group_id, 0);
        let prefix = keys::raft_log_prefix(raft_group_id);
        if let Some((key, _)) = box_try!(self.seek(&seek_key)) {
            if !key.starts_with(&prefix) {
                // No raft logs for the raft group.
                return Ok(());
            }
            let first_index = match keys::raft_log_index(&key) {
                Ok(index) => index,
                Err(_) => return Ok(()),
            };
            for index in first_index..=state.last_index {
                let key = keys::raft_log_key(raft_group_id, index);
                box_try!(batch.delete(&key));
            }
        }
        Ok(())
    }

    fn append(&self, raft_group_id: u64, entries: Vec<Entry>) -> Result<usize> {
        self.append_slice(raft_group_id, &entries)
    }

    fn append_slice(&self, raft_group_id: u64, entries: &[Entry]) -> Result<usize> {
        let mut wb = SkiplistWriteBatch::new(self);
        let buf = Vec::with_capacity(1024);
        wb.append_impl(raft_group_id, entries, buf)?;
        self.consume(&mut wb, false)
    }

    fn put_raft_state(&self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        box_try!(self.put_msg(&keys::raft_state_key(raft_group_id), state));
        Ok(())
    }

    fn gc(&self, raft_group_id: u64, mut from: u64, to: u64) -> Result<usize> {
        if from >= to {
            return Ok(0);
        }
        if from == 0 {
            let start_key = keys::raft_log_key(raft_group_id, 0);
            let prefix = keys::raft_log_prefix(raft_group_id);
            match box_try!(self.seek(&start_key)) {
                Some((k, _)) if k.starts_with(&prefix) => from = box_try!(keys::raft_log_index(&k)),
                // No need to gc.
                _ => return Ok(0),
            }
        }

        let mut raft_wb = self.write_batch_with_cap(MAX_DELETE_BATCH_SIZE);
        for idx in from..to {
            let key = keys::raft_log_key(raft_group_id, idx);
            box_try!(raft_wb.delete(&key));
            if raft_wb.data_size() >= MAX_DELETE_BATCH_SIZE {
                // Avoid large write batch to reduce latency.
                self.write(&raft_wb).unwrap();
                raft_wb.clear();
            }
        }

        // TODO: disable WAL here.
        if !Mutable::is_empty(&raft_wb) {
            self.write(&raft_wb).unwrap();
        }
        Ok((to - from) as usize)
    }

    fn has_builtin_entry_cache(&self) -> bool {
        false
    }

    fn flush_stats(&self) -> CacheStats {
        CacheStats::default()
    }
}

impl RaftLogBatch for SkiplistWriteBatch {
    fn append(&mut self, raft_group_id: u64, entries: Vec<Entry>) -> Result<()> {
        self.append_slice(raft_group_id, &entries)
    }

    fn append_slice(&mut self, raft_group_id: u64, entries: &[Entry]) -> Result<()> {
        if let Some(max_size) = entries.iter().map(|e| e.compute_size()).max() {
            let ser_buf = Vec::with_capacity(max_size as usize);
            return self.append_impl(raft_group_id, entries, ser_buf);
        }
        Ok(())
    }

    fn cut_logs(&mut self, raft_group_id: u64, from: u64, to: u64) {
        for index in from..to {
            let key = keys::raft_log_key(raft_group_id, index);
            self.delete(&key).unwrap();
        }
    }

    fn put_raft_state(&mut self, raft_group_id: u64, state: &RaftLocalState) -> Result<()> {
        box_try!(self.put_msg(&keys::raft_state_key(raft_group_id), state));
        Ok(())
    }

    fn is_empty(&self) -> bool {
        Mutable::is_empty(self)
    }
}

impl SkiplistWriteBatch {
    fn append_impl(
        &mut self,
        raft_group_id: u64,
        entries: &[Entry],
        mut ser_buf: Vec<u8>,
    ) -> Result<()> {
        for entry in entries {
            let key = keys::raft_log_key(raft_group_id, entry.get_index());
            ser_buf.clear();
            entry.write_to_vec(&mut ser_buf).unwrap();
            box_try!(self.put(&key, &ser_buf));
        }
        Ok(())
    }
}
