use crate::{
    ast,
    check::{self, DiagnosticUpdate, ErrorsAcc},
    config,
    document::*,
    inlays::InlayHandler,
    parse,
    semantic::*,
    smt, util,
};
use dashmap::DashMap;
use hashbrown::HashMap;
use log::info;
use ropey::Rope;
use std::{
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::SystemTime,
};
use tokio::{
    select, spawn,
    sync::{broadcast, mpsc, oneshot, watch},
    time::Instant,
};
use tower_lsp::lsp_types::*;
use util::Result;
enum DraftMsg {
    Delete(Instant),
    Update(DidChangeTextDocumentParams, Instant),
    Snapshot(oneshot::Sender<Draft>),
    Shutdown,
}

async fn make_red_tree(draft: Draft, uri: Url, tx_link: mpsc::Sender<LinkMsg>) {
    info!("update red tree {uri}");
    match draft {
        Draft::UVL {
            timestamp,
            source,
            tree,
        } => {
            let mut ast = ast::visit_root(source.clone(), tree.clone(), uri.clone(), timestamp);
            ast.errors.append(&mut check::check_sanity(&tree, &source));
            ast.errors.append(&mut check::check_errors(&tree, &source));
            let _ = tx_link.send(LinkMsg::UpdateAst(Arc::new(ast))).await;
        }
        Draft::JSON {
            tree,
            source,
            timestamp,
        } => {
            let _ = tx_link
                .send(LinkMsg::UpdateConfig(Arc::new(config::parse_json(
                    tree, source, uri, timestamp,
                ))))
                .await;
        }
    }
}
async fn draft_handler(
    mut rx: mpsc::UnboundedReceiver<DraftMsg>,
    uri: Url,
    initial_text: String,
    tx_link: mpsc::Sender<LinkMsg>,
    initial_timestamp: Instant,
) {
    let rope = Rope::from_str(&initial_text);
    let mut draft = if util::is_config(&uri) {
        Draft::JSON {
            tree: parse::parse_json(&rope, None),
            source: Rope::from_str(&initial_text),
            timestamp: initial_timestamp,
        }
    } else {
        Draft::UVL {
            tree: parse::parse(&rope, None),
            source: rope,
            timestamp: initial_timestamp,
        }
    };
    info!("started draft handler {uri}");
    spawn(make_red_tree(draft.clone(), uri.clone(), tx_link.clone()));
    while let Some(msg) = rx.recv().await {
        match msg {
            DraftMsg::Delete(timestamp) => {
                let _ = tx_link.send(LinkMsg::Delete(uri.clone(), timestamp)).await;

                break;
            }
            DraftMsg::Shutdown => {
                break;
            }
            DraftMsg::Update(params, timestamp) => {
                draft = match draft {
                    Draft::UVL {
                        mut source,
                        mut tree,
                        ..
                    } => {
                        let whole_file = update_text(&mut source, Some(&mut tree), params);
                        Draft::UVL {
                            timestamp,
                            tree: parse::parse(
                                &source,
                                if whole_file { None } else { Some(&tree) },
                            ),
                            source,
                        }
                    }
                    Draft::JSON {
                        mut source,
                        mut tree,
                        ..
                    } => {
                        let whole_file = update_text(&mut source, Some(&mut tree), params);
                        Draft::JSON {
                            timestamp,
                            tree: parse::parse_json(
                                &source,
                                if whole_file { None } else { Some(&tree) },
                            ),
                            source,
                        }
                    }
                };
                spawn(make_red_tree(draft.clone(), uri.clone(), tx_link.clone()));
            }
            DraftMsg::Snapshot(out) => {
                let _ = out.send(draft.clone());
            }
        }
    }
}

struct DraftState {
    handler: mpsc::UnboundedSender<DraftMsg>,
    state: DocumentState,
    timestamp: Instant,
}
enum LinkMsg {
    Delete(Url, Instant),
    UpdateAst(Arc<ast::AstDocument>),
    UpdateConfig(Arc<config::ConfigDocument>),
    Shutdown,
}

async fn link_handler(
    mut rx: mpsc::Receiver<LinkMsg>,
    tx_cache: watch::Sender<Arc<RootGraph>>,
    tx_err: mpsc::Sender<DiagnosticUpdate>,
) {
    let mut latest_configs: HashMap<FileID, Arc<config::ConfigDocument>> = HashMap::new();
    let mut latest_ast: HashMap<FileID, Arc<ast::AstDocument>> = HashMap::new();
    let mut timestamps: HashMap<Url, Instant> = HashMap::new();
    let (tx_execute, rx_execute) = watch::channel((latest_ast.clone(), latest_configs.clone(), 0));
    let mut dirty = false;
    let mut revision = 0;
    info!("started link handler");
    spawn(link_executor(rx_execute, tx_cache, tx_err));
    let mut timer = tokio::time::interval(tokio::time::Duration::from_millis(100));
    loop {
        select! {
            Some(msg)=rx.recv()=>{
                match msg{
                    LinkMsg::Shutdown=>{
                        break;
                    }
                    LinkMsg::Delete(uri,timestamp)=>{
                        if timestamps.get(&uri).map(|old|old < &timestamp).unwrap_or(true){
                            let id = FileID::new(uri.as_str());
                            latest_ast.remove(&id);
                            latest_configs.remove(&id);
                            timestamps.insert(uri,timestamp);
                        }
                        revision +=1;
                        dirty=true;
                    }
                    LinkMsg::UpdateAst(ast)=>{
                        if timestamps.get(&ast.uri).map(|old|old < &ast.timestamp).unwrap_or(true){
                            timestamps.insert(ast.uri.clone(),ast.timestamp);
                            let id = FileID::new(ast.uri.as_str());
                            latest_ast.insert(id,ast);
                        }

                        revision +=1;
                        dirty=true;

                    }
                    LinkMsg::UpdateConfig(conf)=>{
                        if timestamps.get(&conf.uri).map(|old|old < &conf.timestamp).unwrap_or(true){
                            timestamps.insert(conf.uri.clone(),conf.timestamp);
                            let id = FileID::new(conf.uri.as_str());
                                latest_configs.insert(id,conf);


                        }
                        revision +=1;
                        dirty=true;

                    }


                }
            }
            _=timer.tick()=>{
                if dirty{
                    info!("link prepare");
                    dirty=false;
                    tx_execute.send_modify(|old|*old=(latest_ast.clone(),latest_configs.clone(),revision))
                }
            }
            else =>{
                break;

            }

        }
    }
    async fn link_executor(
        mut rx: watch::Receiver<(
            HashMap<FileID, Arc<ast::AstDocument>>,
            HashMap<FileID, Arc<config::ConfigDocument>>,
            u64,
        )>,
        tx_cache: watch::Sender<Arc<RootGraph>>,
        tx_err: mpsc::Sender<DiagnosticUpdate>,
    ) {
        let mut timestamps: HashMap<FileID, Instant> = HashMap::new();
        info!("started link execute");
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            info!("link execute");
            tx_cache.borrow().cancel();
            let (ast, configs, revison) = (*rx.borrow_and_update()).clone();
            let mut err = ErrorsAcc {
                files: &ast,
                configs: &configs,
                errors: HashMap::new(),
            };
            let old = tx_cache.borrow().cache().clone();
            let root = RootGraph::new(&ast, &configs, revison, &old, &mut err, &mut timestamps);

            let _ = tx_cache.send(Arc::new(root));
            let _ = tx_err
                .send(DiagnosticUpdate {
                    timestamp: revison,
                    error_state: err.errors,
                })
                .await;
        }
    }
}

#[derive(Clone)]
pub struct AsyncPipeline {
    drafts: Arc<DashMap<Url, DraftState>>,
    tx_link: mpsc::Sender<LinkMsg>,
    tx_err: mpsc::Sender<DiagnosticUpdate>,
    rx_root: watch::Receiver<Arc<RootGraph>>,
    tx_dirty_tree: broadcast::Sender<()>,
    revision_counter: Arc<AtomicU64>,
    client: tower_lsp::Client,
    inlay_handler: InlayHandler,
}
impl AsyncPipeline {
    pub fn new(client: tower_lsp::Client) -> Self {
        let (tx_link, rx_link) = mpsc::channel(1024);
        let (tx_root, rx_root) = watch::channel(Arc::new(RootGraph::default()));
        let (tx_err, rx_err) = mpsc::channel(1024);
        let revision_counter = Arc::new(AtomicU64::new(0));
        let (tx_dirty, _) = broadcast::channel(1024);
        let inlay_handler = InlayHandler::new(client.clone());
        spawn(link_handler(rx_link, tx_root, tx_err.clone()));
        spawn(check::diagnostic_handler(rx_err, client.clone()));
        spawn(smt::check_handler(
            rx_root.clone(),
            tx_err.clone(),
            client.clone(),
            inlay_handler.clone(),
        ));
        AsyncPipeline {
            inlay_handler,
            client,
            tx_dirty_tree: tx_dirty,
            revision_counter,
            drafts: Arc::new(DashMap::new()),
            tx_link,
            tx_err,
            rx_root,
        }
    }
    pub fn touch(&self, uri: &Url) {
        self.update(DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier {
                uri: uri.clone(),
                version: 0,
            },
            content_changes: Vec::new(),
        })
    }
    pub fn inlay_state(&self) -> &InlayHandler {
        &self.inlay_handler
    }

    pub fn client(&self) -> tower_lsp::Client {
        self.client.clone()
    }
    pub async fn update_config(&self, doc: config::ConfigDocument, intial: bool) {
        self.revision_counter.fetch_add(1, Ordering::SeqCst);
        if intial {
            let _ = self.tx_dirty_tree.send(());
        }
        let _ = self
            .tx_link
            .send(LinkMsg::UpdateConfig(Arc::new(doc)))
            .await;
    }
    pub fn subscribe_dirty_tree(&self) -> broadcast::Receiver<()> {
        self.tx_dirty_tree.subscribe()
    }
    pub fn open(&self, uri: Url, text: String, state: DocumentState) {
        match self.drafts.entry(uri.clone()) {
            dashmap::mapref::entry::Entry::Vacant(e) => {
                let timestamp = Instant::now();
                self.revision_counter.fetch_add(1, Ordering::SeqCst);
                let _ = self.tx_dirty_tree.send(());
                let (tx, rx) = mpsc::unbounded_channel();
                spawn(draft_handler(
                    rx,
                    uri,
                    text,
                    self.tx_link.clone(),
                    timestamp,
                ));
                e.insert(DraftState {
                    handler: tx,
                    state,
                    timestamp,
                });
            }
            dashmap::mapref::entry::Entry::Occupied(mut e) => {
                if e.get().state.can_update(&state) {
                    let timestamp = Instant::now();

                    self.revision_counter.fetch_add(1, Ordering::SeqCst);
                    let _ = self.tx_dirty_tree.send(());
                    let (tx, rx) = mpsc::unbounded_channel();
                    spawn(draft_handler(
                        rx,
                        uri,
                        text,
                        self.tx_link.clone(),
                        timestamp,
                    ));
                    e.insert(DraftState {
                        handler: tx,
                        state,
                        timestamp,
                    });
                }
            }
        }
    }
    pub fn should_load(&self, uri: &Url, time: SystemTime) -> bool {
        self.drafts
            .get(uri)
            .map(|i| i.state.can_update(&DocumentState::OwnedByOs(time)))
            .unwrap_or(true)
    }
    pub fn stat(&self, uri: &Url) -> Option<(Instant, DocumentState)> {
        self.drafts.get(uri).map(|i| (i.timestamp, i.state.clone()))
    }
    pub async fn delete(&self, uri: &Url, state: DocumentState) {
        if let Some((_, _old)) = self
            .drafts
            .remove_if(uri, |_, v| v.state.can_update(&state))
        {
            self.revision_counter.fetch_add(1, Ordering::SeqCst);
            let _ = self.tx_dirty_tree.send(());
            let _ = self
                .tx_link
                .send(LinkMsg::Delete(uri.clone(), Instant::now()))
                .await;
        }
    }
    pub fn update(&self, params: DidChangeTextDocumentParams) {
        if let Some(state) = self.drafts.get(&params.text_document.uri) {
            self.revision_counter.fetch_add(1, Ordering::SeqCst);
            let _ = self.tx_dirty_tree.send(());
            let _ = state.handler.send(DraftMsg::Update(params, Instant::now()));
        }
    }
    pub async fn snapshot_draft(&self, uri: &Url) -> Result<Option<Draft>> {
        if let Some(state) = self.drafts.get(uri) {
            let (tx, rx) = oneshot::channel();
            let _ = state.handler.send(DraftMsg::Snapshot(tx));
            Ok(Some(rx.await?))
        } else {
            Ok(None)
        }
    }
    pub async fn snapshot_root(&self, uri: &Url) -> Result<Arc<RootGraph>> {
        let time = Instant::now();
        let mut rx = self.rx_root.clone();

        loop {
            {
                let state = rx.borrow_and_update();
                if state.containes(uri) {
                    info!("waited {:?} for root", time.elapsed());
                    return Ok(state.clone());
                }
            }
            rx.changed().await?;
        }
    }
    pub fn root(&self) -> watch::Receiver<Arc<RootGraph>> {
        self.rx_root.clone()
    }
    pub async fn snapshot_root_sync(
        &self,
        uri: &Url,
        timestamp: Instant,
    ) -> Result<Arc<RootGraph>> {
        let mut rx = self.rx_root.clone();

        loop {
            {
                let state = rx.borrow_and_update();
                if state
                    .timestamp(uri)
                    .map(|t| timestamp <= t)
                    .unwrap_or(false)
                {
                    return Ok(state.clone());
                }
            }
            rx.changed().await?;
        }
    }
    pub async fn sync_root<F: Fn(&RootGraph) -> bool>(&self, f: F) -> Result<Arc<RootGraph>> {
        let mut rx = self.rx_root.clone();
        loop {
            {
                let state = rx.borrow_and_update();
                if f(&state) {
                    return Ok(state.clone());
                }
            }
            rx.changed().await?;
        }
    }

    pub async fn sync_root_global(&self) -> Result<Arc<RootGraph>> {
        let mut rx = self.rx_root.clone();
        loop {
            {
                let state = rx.borrow_and_update();
                info!(
                    "sync {} {}",
                    state.revision(),
                    self.revision_counter.load(Ordering::SeqCst)
                );
                if self.revision_counter.load(Ordering::SeqCst) <= state.revision() {
                    return Ok(state.clone());
                }
            }
            rx.changed().await?;
        }
    }
    pub async fn snapshot(&self, uri: &Url, sync: bool) -> Result<Option<(Draft, Arc<RootGraph>)>> {
        let time = Instant::now();
        if let Some(draft) = self.snapshot_draft(uri).await? {
            info!("waited {:?} for draft", time.elapsed());
            Ok(Some(if sync {
                let timestamp = draft.timestamp();
                (draft, self.snapshot_root_sync(uri, timestamp).await?)
            } else {
                (draft, self.snapshot_root(uri).await?)
            }))
        } else {
            Ok(None)
        }
    }
}