use std::fs::File;
use std::io::BufWriter;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Error, Result};
use futures::{compat::Future01CompatExt, FutureExt, TryFutureExt};
use futures01::future::{Either, Future, IntoFuture};
use sentry::configure_scope;
use sentry::integrations::failure::capture_fail;
use symbolic::common::{Arch, ByteView};
use symbolic::symcache::{self, SymCache, SymCacheWriter};

use crate::actors::common::cache::{CacheItemRequest, CachePath, Cacher};
use crate::actors::objects::{FindObject, ObjectFile, ObjectFileMeta, ObjectPurpose, ObjectsActor};
use crate::cache::{Cache, CacheKey, CacheStatus};
use crate::sources::{FileType, SourceConfig};
use crate::types::{ObjectFeatures, ObjectId, ObjectType, Scope};
use crate::utils::futures::ThreadPool;
use crate::utils::sentry::{SentryFutureExt, WriteSentryScope};

/*#[derive(Fail, Debug, Clone, Copy)]
pub enum SymCacheErrorKind {
    #[fail(display = "failed to write symcache")]
    Io,

    #[fail(display = "failed to download object")]
    Fetching,

    #[fail(display = "failed to parse symcache")]
    Parsing,

    #[fail(display = "failed to parse object")]
    ObjectParsing,

    #[fail(display = "symcache building took too long")]
    Timeout,

    #[fail(display = "computation was canceled internally")]
    Canceled,
}

symbolic::common::derive_failure!(
    SymCacheError,
    SymCacheErrorKind,
    doc = "Errors happening while generating a symcache"
);

impl From<io::Error> for SymCacheError {
    fn from(e: io::Error) -> Self {
        e.context(SymCacheErrorKind::Io).into()
    }
}*/

#[derive(Clone, Debug)]
pub struct SymCacheActor {
    symcaches: Arc<Cacher<FetchSymCacheInternal>>,
    objects: ObjectsActor,
    threadpool: ThreadPool,
}

impl SymCacheActor {
    pub fn new(cache: Cache, objects: ObjectsActor, threadpool: ThreadPool) -> Self {
        SymCacheActor {
            symcaches: Arc::new(Cacher::new(cache)),
            objects,
            threadpool,
        }
    }
}

#[derive(Clone, Debug)]
pub struct SymCacheFile {
    object_type: ObjectType,
    identifier: ObjectId,
    scope: Scope,
    data: ByteView<'static>,
    features: ObjectFeatures,
    status: CacheStatus,
    arch: Arch,
}

impl SymCacheFile {
    pub fn parse(&self) -> Result<Option<SymCache<'_>>> {
        match self.status {
            CacheStatus::Negative => Ok(None),
            CacheStatus::Malformed => Err(anyhow::anyhow!("Failed to parse object")),
            CacheStatus::Positive => Ok(Some(
                SymCache::parse(&self.data).context("Failed to parse symcache")?,
            )),
        }
    }

    /// Returns the architecture of this symcache.
    pub fn arch(&self) -> Arch {
        self.arch
    }

    /// Returns the features of the object file this symcache was constructed from.
    pub fn features(&self) -> ObjectFeatures {
        self.features
    }
}

#[derive(Clone, Debug)]
struct FetchSymCacheInternal {
    request: FetchSymCache,
    objects_actor: ObjectsActor,
    object_meta: Arc<ObjectFileMeta>,
    threadpool: ThreadPool,
}

impl CacheItemRequest for FetchSymCacheInternal {
    type Item = SymCacheFile;
    type Error = Error;

    fn get_cache_key(&self) -> CacheKey {
        self.object_meta.cache_key()
    }

    fn compute(&self, path: &Path) -> Box<dyn Future<Item = CacheStatus, Error = Self::Error>> {
        let path = path.to_owned();
        let object = self
            .objects_actor
            .fetch(self.object_meta.clone())
            .map_err(|e| e.context("Failed to download object"));

        let threadpool = self.threadpool.clone();
        let result = object.and_then(move |object| {
            let future = futures01::lazy(move || {
                if object.status() != CacheStatus::Positive {
                    return Ok(object.status());
                }

                let status = if let Err(e) = write_symcache(&path, &*object) {
                    log::warn!("Failed to write symcache: {}", e);
                    capture_fail(e.cause().unwrap_or(&e));

                    CacheStatus::Malformed
                } else {
                    CacheStatus::Positive
                };

                Ok(status)
            });

            threadpool
                .spawn_handle(future.sentry_hub_current().compat())
                .boxed_local()
                .compat()
                .map_err(|e| e.context("Computation was canceled internally"))
                .flatten()
        });

        let num_sources = self.request.sources.len();

        Box::new(future_metrics!(
            "symcaches",
            Some((Duration::from_secs(1200), anyhow::anyhow!("Symcache building took too long"))),
            result,
            "num_sources" => &num_sources.to_string()
        ))
    }

    fn should_load(&self, data: &[u8]) -> bool {
        SymCache::parse(data)
            .map(|symcache| symcache.is_latest())
            .unwrap_or(false)
    }

    fn load(
        &self,
        scope: Scope,
        status: CacheStatus,
        data: ByteView<'static>,
        _: CachePath,
    ) -> Self::Item {
        // TODO: Figure out if this double-parsing could be avoided
        let arch = SymCache::parse(&data)
            .map(|cache| cache.arch())
            .unwrap_or_default();

        SymCacheFile {
            object_type: self.request.object_type,
            identifier: self.request.identifier.clone(),
            scope,
            data,
            features: self.object_meta.features(),
            status,
            arch,
        }
    }
}

/// Information for fetching the symbols for this symcache
#[derive(Debug, Clone)]
pub struct FetchSymCache {
    pub object_type: ObjectType,
    pub identifier: ObjectId,
    pub sources: Arc<Vec<SourceConfig>>,
    pub scope: Scope,
}

impl SymCacheActor {
    pub fn fetch(
        &self,
        request: FetchSymCache,
    ) -> impl Future<Item = Arc<SymCacheFile>, Error = Arc<Error>> {
        let object = self
            .objects
            .find(FindObject {
                filetypes: FileType::from_object_type(request.object_type),
                identifier: request.identifier.clone(),
                sources: request.sources.clone(),
                scope: request.scope.clone(),
                purpose: ObjectPurpose::Debug,
            })
            .map_err(|e| Arc::new(e.context("Failed to download object")));

        let symcaches = self.symcaches.clone();
        let threadpool = self.threadpool.clone();
        let objects = self.objects.clone();

        let object_type = request.object_type;
        let identifier = request.identifier.clone();
        let scope = request.scope.clone();

        object.and_then(move |object| {
            object
                .map(move |object| {
                    Either::A(symcaches.compute_memoized(FetchSymCacheInternal {
                        request,
                        objects_actor: objects,
                        object_meta: object,
                        threadpool,
                    }))
                })
                .unwrap_or_else(move || {
                    Either::B(
                        Ok(Arc::new(SymCacheFile {
                            object_type,
                            identifier,
                            scope,
                            data: ByteView::from_slice(b""),
                            features: ObjectFeatures::default(),
                            status: CacheStatus::Negative,
                            arch: Arch::Unknown,
                        }))
                        .into_future(),
                    )
                })
        })
    }
}

fn write_symcache(path: &Path, object: &ObjectFile) -> Result<()> {
    configure_scope(|scope| {
        scope.set_transaction(Some("compute_symcache"));
        object.write_sentry_scope(scope);
    });

    let symbolic_object = object.parse().context("Failed to parse object")?.unwrap();

    let file = File::create(&path).context("Failed to write symcache")?;
    let mut writer = BufWriter::new(file);

    log::debug!("Converting symcache for {}", object.cache_key());

    if let Err(e) = SymCacheWriter::write_object(&symbolic_object, &mut writer) {
        match e.kind() {
            symcache::SymCacheErrorKind::WriteFailed => {
                return Err(e.context("Failed to write symcache"))
            }
            _ => return Err(e.context("Failed to parse object")),
        }
    }

    let file = writer.into_inner().context("Failed to write symcache")?;
    file.sync_all().context("Failed to write symcache")?;

    Ok(())
}
