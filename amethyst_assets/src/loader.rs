use std::any::{Any, TypeId};
use std::borrow::Borrow;
use std::hash::Hash;
use std::path::PathBuf;
use std::sync::Arc;

use fnv::FnvHashMap;
use futures::{Async, Future, IntoFuture, Poll};
use futures::sync::oneshot::{Receiver, channel};
use rayon::ThreadPool;

use asset::AssetSpec;

use store::AnyStore;
use {Allocator, Asset, AssetFuture, BoxedErr, Context, Directory, Format, AssetError, LoadError, Store};

/// Represents a future value of an asset. This future may be
/// added to the ECS world, where the responsible system can poll it and merge
/// it into the persistent storage once it is `Ready`.
pub struct SpawnedFuture<A, E>(Receiver<Result<A, E>>);

impl<A: 'static, E: 'static> SpawnedFuture<A, E> {

    /// Creates a SpawnedFuture and starts processing it.
    pub fn spawn<F>(pool: &ThreadPool, f: F) -> Self
        where A: Send,
              E: Send,
              F: FnOnce() -> Result<A, E> + Send + 'static
    {
        let (send, recv) = channel();

        pool.spawn(move || {
            let res = f();
            let _ = send.send(res);
        });

        SpawnedFuture(recv)
    }
}

impl<A, E> Future for SpawnedFuture<A, E> {
    type Item = A;
    type Error = E;

    fn poll(&mut self) -> Poll<Self::Item, Self::Error> {
        match self.0.poll().expect("Sender destroyed") {
            Async::Ready(x) => x.map(Async::Ready),
            Async::NotReady => Ok(Async::NotReady),
        }
    }
}

/// The asset loader, holding the contexts,
/// the default (directory) store and a reference to the
/// `ThreadPool`.
pub struct Loader {
    contexts: FnvHashMap<TypeId, Box<Any + Send + Sync>>,
    directory: Arc<AnyStore>,
    pool: Arc<ThreadPool>,
    stores: FnvHashMap<String, Arc<AnyStore>>,
}

impl Loader {
    /// Creates a new asset loader, initializing the directory store with the
    /// given path.
    pub fn new<P>(alloc: &Allocator, directory: P, pool: Arc<ThreadPool>) -> Self
        where P: Into<PathBuf>
    {
        Loader {
            contexts: Default::default(),
            directory: Arc::new(Directory::new(alloc, directory)),
            pool: pool,
            stores: Default::default(),
        }
    }

    /// Adds a store which can later be loaded from by supplying the same `name`
    /// to `load_from`.
    pub fn add_store<I, S>(&mut self, name: I, store: S)
        where I: Into<String>,
              S: Store + Send + Sync + 'static
    {
        self.stores.insert(name.into(), Arc::new(store));
    }

    /// Registers an asset and inserts a context into the map.
    pub fn register<A, C>(&mut self, context: C)
        where A: Asset + 'static,
              C: Context<Asset=A>,
    {
        self.contexts
            .insert(TypeId::of::<A>(), Box::new(Arc::new(context)));
    }

    /// Like `load_from`, but doesn't ask the cache for the asset.
    pub fn reload<A, F, N, S>(&self,
                              name: N,
                              format: F,
                              store: &S)
                              -> AssetFuture<A>
        where A: Asset,
              F: Format<Data=<A::Context as Context>::Data> + 'static,
              F::Error: 'static,
              N: Into<String>,
              S: Eq + Hash + ? Sized,
              String: Borrow<S>
    {
        let context = self.context::<A::Context>();

        reload_asset::<A, F, N, _>(context.clone(),
                                   format,
                                   name,
                                   self.store(store),
                                   &self.pool)
    }

    /// Loads an asset with a given format from the default (directory) store.
    /// If you want to load from a custom source instead, use `load_from`.
    ///
    /// The actual work is done on a worker thread, thus this method immediately returns
    /// a future.
    pub fn load<A, F, N>(&self,
                         id: N,
                         format: F)
                         -> AssetFuture<A>
        where A: Asset,
              F: Format<Data=<A::Context as Context>::Data> + 'static,
              N: Into<String>
    {
        self.load_from::<A, F, _, _>(id, format, "")
    }

    /// Loads an asset with a given id and format from a custom store.
    /// The actual work is done on a worker thread, thus this method immediately returns
    /// a future.
    ///
    /// # Panics
    ///
    /// Panics if the asset wasn't registered.
    pub fn load_from<A, F, N, S>(&self,
                                 name: N,
                                 format: F,
                                 store: &S)
                                 -> AssetFuture<A>
        where A: Asset,
              F: Format<Data=<A::Context as Context>::Data> + 'static,
              N: Into<String>,
              S: AsRef<str> + Eq + Hash + ? Sized,
              String: Borrow<S>
    {
        let context = self.context::<A::Context>();
        let store = match store.as_ref() {
            "" => &self.directory,
            _ => self.store(store),
        };

        load_asset::<A, F, N, _>(context.clone(), format, name, store, &self.pool)
    }

    fn context<C>(&self) -> &Arc<C>
        where C: Context,
    {
        let context = self.contexts
            .get(&TypeId::of::<C::Asset>())
            .expect("Assets need to be registered with `Loader::register`.");

        // `Any + Send + Sync` doesn't have `downcast_ref`
        Any::downcast_ref(context).unwrap()
    }

    fn store<S>(&self, store: &S) -> &Arc<AnyStore>
        where S: Eq + Hash + ? Sized,
              String: Borrow<S>
    {
        self.stores
            .get(&store)
            .expect("No such store. Maybe you forgot to add it with `Loader::add_store`?")
    }
}

/// Loads an asset with a given context, format, specifier and storage right now.
pub fn load_asset<A, F, N, S>(context: Arc<A::Context>,
                              format: F,
                              name: N,
                              storage: &S,
                              pool: &Arc<ThreadPool>)
                              -> AssetFuture<A>
    where A: Asset,
          A::Context: Context,
          F: Format<Data=<A::Context as Context>::Data> + 'static,
          F::Error: 'static,
          N: Into<String>,
          S: Store,
          <S::Result as IntoFuture>::Future: 'static,
{
    let name = name.into();

    let store_id = storage.store_id();
    let spec = AssetSpec::new(name.clone(), F::extension(), store_id);

    context.retrieve(&spec)
        .unwrap_or_else(move || load_asset_inner(context, format, spec, storage, pool))

}

/// Loads an asset with a given context, format, specifier and storage right now.
/// Note that this method does not ask for a cached version of the asset, but just
/// reloads the asset.
pub fn reload_asset<A, F, N, S>(context: Arc<A::Context>,
                                format: F,
                                name: N,
                                storage: &S,
                                pool: &Arc<ThreadPool>)
                                -> AssetFuture<A>
    where A: Asset,
          A::Context: Context,
          //<A::Context as Context>::Error: 'static,
          F: Format<Data=<A::Context as Context>::Data> + 'static,
          F::Error: 'static,
          N: Into<String>,
          S: Store,
          <S::Result as IntoFuture>::Future: 'static,
{
    let name = name.into();

    let store_id = storage.store_id();
    let spec = AssetSpec::new(name.clone(), F::extension(), store_id);

    load_asset_inner(context, format, spec, storage, pool)
}

fn load_asset_inner<C, F, S>(context: Arc<C>,
                             format: F,
                             spec: AssetSpec,
                             store: &S,
                             pool: &Arc<ThreadPool>)
                             -> AssetFuture<C::Asset>
    where C: Context,
          F: Format<Data=C::Data> + 'static,
          F::Error: 'static,
          S: Store,
          <S::Result as IntoFuture>::Future: 'static,
{
    let spec_store_err = spec.clone();
    let spec_format_err = spec.clone();
    let spec_asset_err = spec.clone();
    let context_clone = context.clone();
    let pool = pool.clone();
    let pool_clone = pool.clone();
    let future = store
        .load(context.category(), &spec.name, spec.ext)
        .into_future()
        .map_err(LoadError::StorageError::<C::Error, F::Error, S::Error>)
        .map_err(BoxedErr::new)
        .map_err(move |e| AssetError::new(spec_store_err, e))
        .and_then(move |bytes| format.parse(bytes, &pool)
            .into_future()
            .map_err(LoadError::FormatError::<C::Error, F::Error, S::Error>)
            .map_err(BoxedErr::new)
            .map_err(move |e| AssetError::new(spec_format_err, e)))
        .and_then(move |data| context.create_asset(data, &pool_clone)
            .into_future()
            .map_err(LoadError::AssetError::<C::Error, F::Error, S::Error>)
            .map_err(BoxedErr::new)
            .map_err(move |e| AssetError::new(spec_asset_err, e)));

    let future: Box<Future<Item=C::Asset, Error=AssetError>> = Box::new(future);
    let future = future.shared();

    context_clone.cache(spec, future.clone());

    future
}
