use std::{
    cell::RefCell,
    collections::{HashSet, VecDeque},
    rc::Rc,
    sync::{Arc, Weak},
};

use super::ffi;

const CAPACITY: usize = 32;

/// FBOs belong to one GL context. An active target keeps its FBO alive after eviction.
#[derive(Debug)]
pub(super) struct Framebuffer {
    pub(super) name: ffi::types::GLuint,
    cleanup: Rc<RefCell<Vec<ffi::types::GLuint>>>,
}

impl Drop for Framebuffer {
    fn drop(&mut self) {
        self.cleanup.borrow_mut().push(self.name);
    }
}

/// FBOs are not shared by EGL contexts. Only the owning renderer drains this queue.
#[derive(Default)]
pub(super) struct FramebufferCleanup {
    pending: Rc<RefCell<Vec<ffi::types::GLuint>>>,
    live: HashSet<ffi::types::GLuint>,
}

impl FramebufferCleanup {
    pub(super) fn register(&mut self, name: ffi::types::GLuint) -> Rc<Framebuffer> {
        self.live.insert(name);
        Rc::new(Framebuffer {
            name,
            cleanup: self.pending.clone(),
        })
    }

    pub(super) fn has_pending(&self) -> bool {
        !self.pending.borrow().is_empty()
    }

    pub(super) fn drain(&mut self, mut delete: impl FnMut(ffi::types::GLuint)) {
        for name in self.pending.borrow_mut().drain(..) {
            if self.live.remove(&name) {
                delete(name);
            }
        }
    }

    pub(super) fn destroy_all(&mut self, mut delete: impl FnMut(ffi::types::GLuint)) {
        self.pending.borrow_mut().clear();
        for name in self.live.drain() {
            delete(name);
        }
    }
}

/// Weak attachment identities avoid retaining buffers or confusing recycled GL names.
pub(super) struct FramebufferCache<T> {
    entries: VecDeque<(Weak<T>, Rc<Framebuffer>)>,
}

impl<T> Default for FramebufferCache<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::new(),
        }
    }
}

impl<T> FramebufferCache<T> {
    pub(super) fn prune(&mut self) {
        self.entries
            .retain(|(attachment, _)| attachment.strong_count() > 0);
    }

    pub(super) fn get(&mut self, attachment: &Arc<T>) -> Option<Rc<Framebuffer>> {
        self.prune();
        let key = Arc::downgrade(attachment);
        let index = self.entries.iter().position(|(entry, _)| entry.ptr_eq(&key))?;
        let entry = self.entries.remove(index).unwrap();
        let framebuffer = entry.1.clone();
        self.entries.push_back(entry);
        Some(framebuffer)
    }

    pub(super) fn insert(&mut self, attachment: &Arc<T>, framebuffer: Rc<Framebuffer>) {
        self.prune();
        if self.entries.len() == CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((Arc::downgrade(attachment), framebuffer));
    }

    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reuse_eviction_and_attachment_lifetime() {
        let mut cleanup = FramebufferCleanup::default();
        let attachment = Arc::new(());
        let mut cache = FramebufferCache::default();
        cache.insert(&attachment, cleanup.register(1));
        let active = cache.get(&attachment).unwrap();
        assert!(Rc::ptr_eq(&active, &cache.get(&attachment).unwrap()));
        let mut live = Vec::new();
        for name in 2..=CAPACITY as u32 + 1 {
            let next = Arc::new(());
            cache.insert(&next, cleanup.register(name));
            live.push(next);
        }
        assert_eq!(cache.entries.len(), CAPACITY);
        assert!(cache.get(&attachment).is_none());
        assert!(!cleanup.has_pending());
        drop(active);
        let mut deleted = Vec::new();
        cleanup.drain(|name| deleted.push(name));
        assert_eq!(deleted, [1]);
        drop(live);
        cache.prune();
        assert!(cache.entries.is_empty());
        cleanup.drain(|name| deleted.push(name));
        assert_eq!(deleted.len(), CAPACITY + 1);
        assert_eq!(Arc::strong_count(&attachment), 1);
    }

    #[test]
    fn cleanup_is_context_local_and_handles_targets_outliving_renderer() {
        let mut first = FramebufferCleanup::default();
        let mut second = FramebufferCleanup::default();
        // GL names may collide between two contexts.
        let first_target = first.register(1);
        let second_target = second.register(1);
        drop(first_target);
        let mut deleted = Vec::new();
        second.drain(|name| deleted.push(name));
        assert!(deleted.is_empty());
        first.drain(|name| deleted.push(name));
        assert_eq!(deleted, [1]);
        second.destroy_all(|name| deleted.push(name));
        assert_eq!(deleted, [1, 1]);
        drop(second_target);
        second.drain(|_| panic!("target drop must not delete twice"));
    }

    #[test]
    fn separate_identity_token_preserves_attachment_uniqueness() {
        struct Attachment {
            identity: Arc<()>,
        }
        let mut attachment = Arc::new(Attachment {
            identity: Arc::new(()),
        });
        let mut cache = FramebufferCache::default();
        let mut cleanup = FramebufferCleanup::default();
        cache.insert(&attachment.identity, cleanup.register(1));
        assert!(Arc::get_mut(&mut attachment).is_some());
        drop(attachment);
        cache.prune();
        assert!(cache.entries.is_empty());
    }
}
