use crate::ann_buf::{AnnBuf, Hash};
use crate::blkmine::HeightWork;
use crate::prooftree;
use packetcrypt_sys::difficulty::pc_degrade_announcement_target;
use rayon::prelude::*;
use std::mem;
use std::sync::{Arc, Mutex, RwLock};
use log::{debug, info, trace, warn};

pub const ANNBUF_SZ: usize = 32 * 1024;
pub type AnnBufSz = AnnBuf<ANNBUF_SZ>;

struct HashTree {
    origin: Arc<Mutex<prooftree::ProofTree>>,
    root_hash: Option<Hash>,
}

impl HashTree {
    fn invalidate(&mut self) {
        if self.root_hash.is_none() {
            return;
        }

        let mut pf = self.origin.lock().unwrap();
        if pf.root_hash.as_ref() == self.root_hash.as_deref() {
            pf.reset();
        }
        self.root_hash = None
    }
}

struct AnnClassMut {
    /// The buffers with the hashes.
    bufs: Vec<Box<AnnBufSz>>,
    topbuf: Option<Box<AnnBufSz>>,

    /// Hash trees which contain announcements in this class.
    /// A hash tree will only care to include either all anns in a class or none
    /// so this is not needed per-AnnBuf.
    dependent_trees: Vec<HashTree>,

    /// Are we currently mining?
    mining: bool,
}

/// AnnClass is a "classification" or grouping of announcements which are similar in their
/// properties (same work done on the ann, same block height when mined).
pub struct AnnClass {
    m: RwLock<AnnClassMut>,

    /// The hash of the current block.
    block_hash: Hash,

    // The type of anns in this class.
    block_height: u32,
    min_ann_work: u32,

    pub id: usize,
}

impl AnnClass {
    pub fn with_bufs(
        bufs: impl Iterator<Item = Box<AnnBufSz>>,
        hw: &HeightWork,
        id: usize,
    ) -> Self {
        // we want topbuf to be the last slice, since it will be the last one to be stolen.
        let mut bufs = bufs.collect::<Vec<_>>();
        let topbuf = bufs.pop().unwrap();
        Self::new(Some(topbuf), bufs, hw, id)
    }

    pub fn new(
        topbuf: Option<Box<AnnBufSz>>,
        bufs: Vec<Box<AnnBufSz>>,
        hw: &HeightWork,
        id: usize,
    ) -> Self {
        AnnClass {
            m: RwLock::new(AnnClassMut {
                bufs,
                topbuf,
                dependent_trees: vec![],
                mining: false,
            }),
            block_hash: Default::default(),
            block_height: hw.block_height as u32,
            min_ann_work: hw.work,
            id,
        }
    }

    pub fn push_anns(
        &self,
        anns: &[&[u8]],
        mut indexes: &[u32],
        buf: Box<AnnBufSz>,
    ) -> (usize, Option<Box<AnnBufSz>>) {
        let mut maybe_buf = Some(buf);
        let mut total_consumed = 0;
        loop {
            {
                let m = self.m.read().unwrap();
                match &m.topbuf {
                    Some(tb) => {
                        let consumed = tb.push_anns(anns, indexes);
                        total_consumed += consumed;
                        if consumed == indexes.len() {
                            return (total_consumed, maybe_buf);
                        }
                        indexes = &indexes[consumed..];
                    }
                    None => (),
                }
            }
            let newbuf = if let Some(buf) = maybe_buf.take() {
                buf
            } else {
                warn!("Not enough buf space to take anns");
                return (total_consumed, None);
            };
            let oldtop = {
                let mut m = self.m.write().unwrap();
                //println!("new buf in [{}]: count: {}", self.id, m.bufs.len());
                match &m.topbuf {
                    Some(tb) => {
                        // Need to double-check
                        let consumed = tb.push_anns(anns, indexes);
                        if consumed > 0 {
                            total_consumed += consumed;
                            if consumed == indexes.len() {
                                return (total_consumed, Some(newbuf));
                            }
                            indexes = &indexes[consumed..];
                        }
                    }
                    None => (),
                }
                mem::replace(&mut m.topbuf, Some(newbuf))
            };
            if let Some(mut oldtop) = oldtop {
                oldtop.lock();
                self.m.write().unwrap().bufs.push(oldtop);
            }
        }
    }

    pub fn is_dead(&self) -> bool {
        let m = self.m.read().unwrap();
        m.bufs.len() == 0 && m.topbuf.is_none()
    }

    pub fn steal_buf(&self) -> Result<Option<Box<AnnBufSz>>, ()> {
        let mut m = self.m.write().unwrap();
        if m.mining {
            return Err(());
        }
        if m.bufs.is_empty() {
            return Ok(m.topbuf.take());
        }

        m.dependent_trees.iter_mut().for_each(|t| t.invalidate());
        m.dependent_trees.clear();
        Ok(m.bufs.pop())
    }

    pub fn ready_anns(&self) -> usize {
        let m = self.m.read().unwrap();
        m.bufs.iter().map(|b| b.next_ann_index()).sum()
    }

    pub fn ready_anns_bufs(&self) -> (usize, usize) {
        let m = self.m.read().unwrap();
        let anns = m.bufs.iter().map(|b| b.next_ann_index()).sum();
        (anns, m.bufs.len())
    }

    pub fn read_ready_anns(&self, mut out: &mut [prooftree::AnnData]) {
        let m = self.m.read().unwrap();
        // split the out buffer into sub-buffers each of which has enough space to hold
        // enough AnnData for each entry in one buf.
        let mut v = Vec::with_capacity(m.bufs.len());
        for b in &m.bufs {
            let this = b.next_ann_index();
            let (data, excess) = out.split_at_mut(this);
            v.push((b, data));
            out = excess;
        }
        // now that they're split, copy the hashes over in parallel.
        v.into_par_iter().for_each(|(buf, out)| {
            buf.read_ready_anns(out);
        });
    }

    /// Get the effective "value" of these anns, result is a compact int
    /// lower numbers = higher value. Announcements degrade in value with age.
    pub fn ann_effective_work(&self, next_block_height: u32) -> u32 {
        if self.block_height + 3 < next_block_height {
            return self.min_ann_work;
        }
        pc_degrade_announcement_target(self.min_ann_work, next_block_height - self.block_height)
    }
}
