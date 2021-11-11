use bumpalo::Bump;
use futures_channel::mpsc::UnboundedSender;
use fxhash::FxHashMap;
use slab::Slab;
use std::cell::{Cell, RefCell};

use crate::innerlude::*;

pub type FcSlot = *const ();

pub struct Heuristic {
    hook_arena_size: usize,
    node_arena_size: usize,
}

// a slab-like arena with stable references even when new scopes are allocated
// uses a bump arena as a backing
//
// has an internal heuristics engine to pre-allocate arenas to the right size
pub(crate) struct ScopeArena {
    bump: Bump,
    scope_counter: Cell<usize>,
    scopes: RefCell<FxHashMap<ScopeId, *mut Scope>>,
    pub heuristics: RefCell<FxHashMap<FcSlot, Heuristic>>,
    free_scopes: RefCell<Vec<*mut Scope>>,
    nodes: RefCell<Slab<*const VNode<'static>>>,
    pub(crate) sender: UnboundedSender<SchedulerMsg>,
}

impl ScopeArena {
    pub fn new(sender: UnboundedSender<SchedulerMsg>) -> Self {
        Self {
            scope_counter: Cell::new(0),
            bump: Bump::new(),
            scopes: RefCell::new(FxHashMap::default()),
            heuristics: RefCell::new(FxHashMap::default()),
            free_scopes: RefCell::new(Vec::new()),
            nodes: RefCell::new(Slab::new()),
            sender,
        }
    }

    pub fn get_scope(&self, id: &ScopeId) -> Option<&Scope> {
        unsafe { self.scopes.borrow().get(id).map(|f| &**f) }
    }

    // this is unsafe
    pub unsafe fn get_scope_raw(&self, id: &ScopeId) -> Option<*mut Scope> {
        self.scopes.borrow().get(id).map(|f| *f)
    }
    // this is unsafe

    pub unsafe fn get_scope_mut(&self, id: &ScopeId) -> Option<&mut Scope> {
        self.scopes.borrow().get(id).map(|s| &mut **s)
    }

    pub fn new_with_key(
        &self,
        fc_ptr: *const (),
        caller: *const dyn Fn(&Scope) -> Element,
        parent_scope: Option<*mut Scope>,
        height: u32,
        subtree: u32,
    ) -> ScopeId {
        let new_scope_id = ScopeId(self.scope_counter.get());
        self.scope_counter.set(self.scope_counter.get() + 1);

        //
        //
        if let Some(old_scope) = self.free_scopes.borrow_mut().pop() {
            let scope = unsafe { &mut *old_scope };
            log::debug!(
                "reusing scope {:?} as {:?}",
                scope.our_arena_idx,
                new_scope_id
            );

            scope.caller = caller;
            scope.parent_scope = parent_scope;
            scope.height = height;
            scope.subtree = Cell::new(subtree);
            scope.our_arena_idx = new_scope_id;

            scope.frames[0].nodes.get_mut().push({
                let vnode = scope.frames[0]
                    .bump
                    .alloc(VNode::Text(scope.frames[0].bump.alloc(VText {
                        dom_id: Default::default(),
                        is_static: false,
                        text: "",
                    })));
                unsafe { std::mem::transmute(vnode as *mut VNode) }
            });

            scope.frames[1].nodes.get_mut().push({
                let vnode = scope.frames[1]
                    .bump
                    .alloc(VNode::Text(scope.frames[1].bump.alloc(VText {
                        dom_id: Default::default(),
                        is_static: false,
                        text: "",
                    })));
                unsafe { std::mem::transmute(vnode as *mut VNode) }
            });

            let r = self.scopes.borrow_mut().insert(new_scope_id, scope);

            assert!(r.is_none());

            new_scope_id
        } else {
            let (node_capacity, hook_capacity) = {
                let heuristics = self.heuristics.borrow();
                if let Some(heuristic) = heuristics.get(&fc_ptr) {
                    (heuristic.node_arena_size, heuristic.hook_arena_size)
                } else {
                    (0, 0)
                }
            };

            let mut frames = [BumpFrame::new(node_capacity), BumpFrame::new(node_capacity)];

            frames[0].nodes.get_mut().push({
                let vnode = frames[0]
                    .bump
                    .alloc(VNode::Text(frames[0].bump.alloc(VText {
                        dom_id: Default::default(),
                        is_static: false,
                        text: "",
                    })));
                unsafe { std::mem::transmute(vnode as *mut VNode) }
            });

            frames[1].nodes.get_mut().push({
                let vnode = frames[1]
                    .bump
                    .alloc(VNode::Text(frames[1].bump.alloc(VText {
                        dom_id: Default::default(),
                        is_static: false,
                        text: "",
                    })));
                unsafe { std::mem::transmute(vnode as *mut VNode) }
            });

            let scope = self.bump.alloc(Scope {
                sender: self.sender.clone(),
                our_arena_idx: new_scope_id,
                parent_scope,
                height,
                frames,
                subtree: Cell::new(subtree),
                is_subtree_root: Cell::new(false),

                caller,
                generation: 0.into(),

                hooks: HookList::new(hook_capacity),
                shared_contexts: Default::default(),

                items: RefCell::new(SelfReferentialItems {
                    listeners: Default::default(),
                    borrowed_props: Default::default(),
                    suspended_nodes: Default::default(),
                    tasks: Default::default(),
                    pending_effects: Default::default(),
                }),
            });

            dbg!(self.scopes.borrow());

            let r = self.scopes.borrow_mut().insert(new_scope_id, scope);

            assert!(r.is_none());
            // .expect(&format!("scope shouldnt exist, {:?}", new_scope_id));

            new_scope_id
        }
    }

    pub fn try_remove(&self, id: &ScopeId) -> Option<()> {
        self.ensure_drop_safety(id);

        log::debug!("removing scope {:?}", id);
        println!("removing scope {:?}", id);

        let scope = unsafe { &mut *self.scopes.borrow_mut().remove(&id).unwrap() };

        // we're just reusing scopes so we need to clear it out
        scope.hooks.clear();
        scope.shared_contexts.get_mut().clear();
        scope.parent_scope = None;
        scope.generation.set(0);
        scope.is_subtree_root.set(false);
        scope.subtree.set(0);

        scope.frames[0].nodes.get_mut().clear();
        scope.frames[1].nodes.get_mut().clear();

        scope.frames[0].bump.reset();
        scope.frames[1].bump.reset();

        let SelfReferentialItems {
            borrowed_props,
            listeners,
            pending_effects,
            suspended_nodes,
            tasks,
        } = scope.items.get_mut();

        borrowed_props.clear();
        listeners.clear();
        pending_effects.clear();
        suspended_nodes.clear();
        tasks.clear();

        self.free_scopes.borrow_mut().push(scope);

        Some(())
    }

    pub fn reserve_node(&self, node: &VNode) -> ElementId {
        let mut els = self.nodes.borrow_mut();
        let entry = els.vacant_entry();
        let key = entry.key();
        let id = ElementId(key);
        let node: *const VNode = node as *const _;
        let node = unsafe { std::mem::transmute::<*const VNode, *const VNode>(node) };
        entry.insert(node);
        id
    }

    pub fn collect_garbage(&self, id: ElementId) {
        self.nodes.borrow_mut().remove(id.0);
    }

    // These methods would normally exist on `scope` but they need access to *all* of the scopes

    /// This method cleans up any references to data held within our hook list. This prevents mutable aliasing from
    /// causing UB in our tree.
    ///
    /// This works by cleaning up our references from the bottom of the tree to the top. The directed graph of components
    /// essentially forms a dependency tree that we can traverse from the bottom to the top. As we traverse, we remove
    /// any possible references to the data in the hook list.
    ///
    /// References to hook data can only be stored in listeners and component props. During diffing, we make sure to log
    /// all listeners and borrowed props so we can clear them here.
    ///
    /// This also makes sure that drop order is consistent and predictable. All resources that rely on being dropped will
    /// be dropped.
    pub(crate) fn ensure_drop_safety(&self, scope_id: &ScopeId) {
        let scope = self.get_scope(scope_id).unwrap();

        let mut items = scope.items.borrow_mut();

        // make sure we drop all borrowed props manually to guarantee that their drop implementation is called before we
        // run the hooks (which hold an &mut Reference)
        // recursively call ensure_drop_safety on all children
        items.borrowed_props.drain(..).for_each(|comp| {
            let scope_id = comp
                .associated_scope
                .get()
                .expect("VComponents should be associated with a valid Scope");

            self.ensure_drop_safety(&scope_id);

            let mut drop_props = comp.drop_props.borrow_mut().take().unwrap();
            drop_props();
        });

        // Now that all the references are gone, we can safely drop our own references in our listeners.
        items
            .listeners
            .drain(..)
            .for_each(|listener| drop(listener.callback.borrow_mut().take()));
    }

    pub(crate) fn run_scope(&self, id: &ScopeId) -> bool {
        let scope = unsafe { &mut *self.get_scope_mut(id).expect("could not find scope") };

        log::debug!("found scope, about to run: {:?}", id);

        // Cycle to the next frame and then reset it
        // This breaks any latent references, invalidating every pointer referencing into it.
        // Remove all the outdated listeners
        self.ensure_drop_safety(id);

        // Safety:
        // - We dropped the listeners, so no more &mut T can be used while these are held
        // - All children nodes that rely on &mut T are replaced with a new reference
        unsafe { scope.hooks.reset() };

        // Safety:
        // - We've dropped all references to the wip bump frame with "ensure_drop_safety"
        unsafe { scope.reset_wip_frame() };

        {
            let mut items = scope.items.borrow_mut();

            // just forget about our suspended nodes while we're at it
            items.suspended_nodes.clear();
            items.tasks.clear();
            items.pending_effects.clear();

            // guarantee that we haven't screwed up - there should be no latent references anywhere
            debug_assert!(items.listeners.is_empty());
            debug_assert!(items.borrowed_props.is_empty());
            debug_assert!(items.suspended_nodes.is_empty());
            debug_assert!(items.tasks.is_empty());
            debug_assert!(items.pending_effects.is_empty());

            // Todo: see if we can add stronger guarantees around internal bookkeeping and failed component renders.
            scope.wip_frame().nodes.borrow_mut().clear();
        }

        let render: &dyn Fn(&Scope) -> Element = unsafe { &*scope.caller };

        if let Some(link) = render(scope) {
            // right now, it's a panic to render a nodelink from another scope
            // todo: enable this. it should (reasonably) work even if it doesnt make much sense
            assert_eq!(link.scope_id.get(), Some(*id));

            // nodelinks are not assigned when called and must be done so through the create/diff phase
            // however, we need to link this one up since it will never be used in diffing
            scope.wip_frame().assign_nodelink(&link);
            debug_assert_eq!(scope.wip_frame().nodes.borrow().len(), 1);

            if !scope.items.borrow().tasks.is_empty() {
                // self.
            }

            // make the "wip frame" contents the "finished frame"
            // any future dipping into completed nodes after "render" will go through "fin head"
            scope.cycle_frame();
            true
        } else {
            false
        }
    }

    // The head of the bumpframe is the first linked NodeLink
    pub fn wip_head(&self, id: &ScopeId) -> &VNode {
        let scope = self.get_scope(id).unwrap();
        let frame = scope.wip_frame();
        let nodes = frame.nodes.borrow();
        let node: &VNode = unsafe { &**nodes.get(0).unwrap() };
        unsafe { std::mem::transmute::<&VNode, &VNode>(node) }
    }

    // The head of the bumpframe is the first linked NodeLink
    pub fn fin_head(&self, id: &ScopeId) -> &VNode {
        let scope = self.get_scope(id).unwrap();
        let frame = scope.fin_frame();
        let nodes = frame.nodes.borrow();
        let node: &VNode = unsafe { &**nodes.get(0).unwrap() };
        unsafe { std::mem::transmute::<&VNode, &VNode>(node) }
    }

    pub fn root_node(&self, id: &ScopeId) -> &VNode {
        self.fin_head(id)
    }
}