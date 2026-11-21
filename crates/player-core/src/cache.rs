//! Общий кэш блоков сетевого источника (SPEC §2.6, решения 38–45).
//!
//! У звука и видео свои курсоры, но хранилище и загрузчик одни: файл качается
//! один раз на источник. Кэш не знает про HTTP — загрузчик приходит колбэком
//! `Fn(offset, len, abort) -> Result<Vec<u8>, FetchError>`, поэтому тесты
//! работают без сети.
//!
//! Контракт загрузчика:
//!
//! - ему дают абсолютное смещение и желаемую длину (не больше блока);
//! - пустой `Vec` означает конец источника;
//! - короткий непустой ответ допустим — следующий запрос продолжит с его
//!   конца;
//! - при `abort == true` запрос устарел (`Seek` или закрытие) и должен быть
//!   прерван как можно скорее. `Drop` кэша ждёт возврата загрузчика, поэтому
//!   игнорировать флаг нельзя.

use std::collections::{HashMap, VecDeque};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::sync::NetworkStats;

/// Ожидание данных дольше этого порога считается медленным чтением.
const SLOW_READ: Duration = Duration::from_millis(100);

/// Как часто курсор проверяет отмену, пока ждёт блок.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// Размеры кэша и опережение загрузчика.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CacheConfig {
    /// Гранула загрузки и вытеснения.
    pub(crate) block: usize,
    /// Верхняя граница памяти под данные.
    pub(crate) capacity: usize,
    /// Насколько далеко загрузчик уходит вперёд самого дальнего курсора.
    pub(crate) read_ahead: u64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            block: 256 * 1024,
            capacity: 64 * 1024 * 1024,
            read_ahead: 8 * 1024 * 1024,
        }
    }
}

/// Почему загрузчик не отдал данные.
#[derive(Debug)]
pub(crate) struct FetchError {
    /// Запрос устарел: кэш отменил его сам (перемотка или закрытие).
    pub(crate) cancelled: bool,
    pub(crate) message: String,
}

impl FetchError {
    pub(crate) fn cancelled() -> Self {
        Self {
            cancelled: true,
            message: String::from("загрузка отменена"),
        }
    }

    pub(crate) fn other(message: impl Into<String>) -> Self {
        Self {
            cancelled: false,
            message: message.into(),
        }
    }
}

type Loader =
    dyn Fn(u64, usize, &AtomicBool) -> Result<Vec<u8>, FetchError> + Send + Sync + 'static;

/// Один загруженный фрагмент источника.
struct Block {
    start: u64,
    data: Vec<u8>,
}

impl Block {
    fn end(&self) -> u64 {
        self.start + self.data.len() as u64
    }
}

/// Общее состояние кэша и курсоров под одним мьютексом.
struct State {
    /// Куски источника по порядку загрузки; первый — самый старый.
    blocks: VecDeque<Block>,
    /// Смещение следующего запроса загрузчика.
    fetch_pos: u64,
    /// Отмена текущего запроса загрузчика.
    abort: Arc<AtomicBool>,
    /// Где загрузчик встретил конец источника, если длина неизвестна.
    eof_at: Option<u64>,
    /// Терминальная ошибка загрузчика: одинакова для всех курсоров.
    error: Option<String>,
    closed: bool,
    /// Был ли хоть один `read`/`seek`: до него загрузчик не тратит трафик.
    demand: bool,
    /// Позиции курсоров: по ним кэш решает, что можно вытеснять.
    cursors: HashMap<u64, Arc<AtomicU64>>,
    /// Сколько байт лежит в кэше прямо сейчас.
    bytes: usize,
}

/// Счётчики сети. Живут на кэше, поэтому два читателя не считают дважды.
#[derive(Debug, Default)]
pub(crate) struct CacheCounters {
    requests: AtomicU64,
    bytes: AtomicU64,
    slow_reads: AtomicU64,
    slow_ms: AtomicU64,
    retries: AtomicU64,
    restarts: AtomicU64,
}

impl CacheCounters {
    /// Один запрос загрузчика: его считает кэш, а повторы — HTTP-слой.
    pub(crate) fn add_request(&self) {
        self.requests.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_bytes(&self, len: usize) {
        self.bytes.fetch_add(len as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_slow_read(&self, elapsed: Duration) {
        self.slow_reads.fetch_add(1, Ordering::Relaxed);
        self.slow_ms
            .fetch_add(elapsed.as_millis() as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_retry(&self) {
        self.retries.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_restart(&self) {
        self.restarts.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> NetworkStats {
        NetworkStats {
            requests: self.requests.load(Ordering::Relaxed),
            bytes: self.bytes.load(Ordering::Relaxed),
            slow_reads: self.slow_reads.load(Ordering::Relaxed),
            slow_ms: self.slow_ms.load(Ordering::Relaxed),
            retries: self.retries.load(Ordering::Relaxed),
            restarts: self.restarts.load(Ordering::Relaxed),
            // Занятость кэша знает только `Cache::network_stats`.
            cache_bytes: 0,
        }
    }
}

struct Inner {
    config: CacheConfig,
    len: Option<u64>,
    loader: Box<Loader>,
    state: Mutex<State>,
    wake: Condvar,
    counters: Arc<CacheCounters>,
    next_cursor: AtomicU64,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.wake
            .wait(state)
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Ждёт событие, но не дольше [`CANCEL_POLL`]: курсору нужно замечать
    /// отмену ядра, пока блок ещё не пришёл.
    fn wait_timeout<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        self.wake
            .wait_timeout(state, CANCEL_POLL)
            .unwrap_or_else(PoisonError::into_inner)
            .0
    }
}

/// Кэш одного источника: создаётся в `Media`, разделяется курсорами.
pub(crate) struct Cache {
    inner: Arc<Inner>,
}

impl Cache {
    /// Запускает кэш и фоновый загрузчик.
    ///
    /// `len` — известный размер источника (`None`, если сервер не сообщил);
    /// без длины нельзя перематывать от конца. `counters` — общие счётчики
    /// источника: кэш считает запросы и байты, HTTP-слой — повторы.
    pub(crate) fn start(
        config: CacheConfig,
        len: Option<u64>,
        counters: Arc<CacheCounters>,
        loader: impl Fn(u64, usize, &AtomicBool) -> Result<Vec<u8>, FetchError> + Send + Sync + 'static,
    ) -> io::Result<Arc<Self>> {
        if config.block == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "блок кэша не может быть пустым",
            ));
        }
        if config.capacity < config.block {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "кэш меньше одного блока",
            ));
        }

        let inner = Arc::new(Inner {
            config,
            len,
            loader: Box::new(loader),
            state: Mutex::new(State {
                blocks: VecDeque::new(),
                fetch_pos: 0,
                abort: Arc::new(AtomicBool::new(false)),
                eof_at: None,
                error: None,
                closed: false,
                demand: false,
                cursors: HashMap::new(),
                bytes: 0,
            }),
            wake: Condvar::new(),
            counters,
            next_cursor: AtomicU64::new(1),
            thread: Mutex::new(None),
        });

        let worker = Arc::clone(&inner);
        let handle = thread::Builder::new()
            .name(String::from("cache-loader"))
            .spawn(move || run_loader(worker))
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("не удалось запустить загрузчик кэша: {error}"),
                )
            })?;
        *inner.thread.lock().unwrap_or_else(PoisonError::into_inner) = Some(handle);

        Ok(Arc::new(Self { inner }))
    }

    /// Новый курсор чтения с общей позицией источника.
    ///
    /// `cancel` — флаг ядра: пока курсор ждёт блок, он проверяет отмену, иначе
    /// `Stop` не смог бы прервать декодер, стоящий в сети.
    pub(crate) fn cursor(self: &Arc<Self>, cancel: Arc<AtomicBool>) -> CacheCursor {
        let position = Arc::new(AtomicU64::new(0));
        let id = self.inner.next_cursor.fetch_add(1, Ordering::SeqCst);
        {
            let mut state = self.inner.lock();
            state.cursors.insert(id, Arc::clone(&position));
        }
        self.inner.wake.notify_all();

        CacheCursor {
            cache: Arc::clone(self),
            id,
            position,
            cancel,
        }
    }

    /// Размер источника, если он известен.
    #[cfg(test)]
    fn len(&self) -> Option<u64> {
        self.inner.len
    }

    /// Счётчики сети и занятость кэша источника.
    pub(crate) fn network_stats(&self) -> NetworkStats {
        let cache_bytes = self.inner.lock().bytes as u64;
        NetworkStats {
            cache_bytes,
            ..self.inner.counters.snapshot()
        }
    }

    /// Останавливает загрузчик и будит все курсоры.
    pub(crate) fn close(&self) {
        {
            let mut state = self.inner.lock();
            state.closed = true;
            state.abort.store(true, Ordering::SeqCst);
        }
        self.inner.wake.notify_all();
    }

    /// Сколько байт сейчас в памяти: тест на лимит.
    #[cfg(test)]
    fn buffered_bytes(&self) -> usize {
        self.inner.lock().bytes
    }
}

impl Drop for Cache {
    fn drop(&mut self) {
        self.close();
        let handle = self
            .inner
            .thread
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(handle) = handle {
            // Загрузчик обязан вернуться по `abort`; здесь только ждём его.
            let _ = handle.join();
        }
    }
}

/// Курсор одного декодера: своя позиция, общий кэш.
pub(crate) struct CacheCursor {
    /// Курсор держит кэш живым, пока его читают.
    cache: Arc<Cache>,
    id: u64,
    position: Arc<AtomicU64>,
    cancel: Arc<AtomicBool>,
}

impl CacheCursor {
    /// Текущая позиция курсора.
    #[cfg(test)]
    pub(crate) fn position(&self) -> u64 {
        self.position.load(Ordering::SeqCst)
    }
}

impl Drop for CacheCursor {
    fn drop(&mut self) {
        {
            let mut state = self.cache.inner.lock();
            state.cursors.remove(&self.id);
        }
        // Позиция курсора больше не мешает вытеснению.
        self.cache.inner.wake.notify_all();
    }
}

impl Read for CacheCursor {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }

        let started = Instant::now();
        let mut waited = false;
        let outcome = {
            let mut state = self.cache.inner.lock();
            if !state.demand {
                // Первое чтение: только теперь загрузчику есть зачем идти в сеть.
                state.demand = true;
                self.cache.inner.wake.notify_all();
            }
            loop {
                if self.cancel.load(Ordering::SeqCst) {
                    break Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "открытие отменено",
                    ));
                }
                if state.closed {
                    break Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "источник закрыт",
                    ));
                }
                if let Some(message) = &state.error {
                    break Err(io::Error::other(message.clone()));
                }

                let pos = self.position.load(Ordering::SeqCst);
                if self.cache.inner.len.is_some_and(|len| pos >= len) {
                    break Ok(0);
                }
                if state.eof_at.is_some_and(|eof| pos >= eof) {
                    break Ok(0);
                }
                if let Some((index, end)) = locate(&state.blocks, pos) {
                    let block = &state.blocks[index];
                    let offset = (pos - block.start) as usize;
                    let read = ((end - pos) as usize).min(buffer.len());
                    buffer[..read].copy_from_slice(&block.data[offset..offset + read]);
                    self.position.store(pos + read as u64, Ordering::SeqCst);
                    break Ok(read);
                }

                if !waited {
                    // Загрузчик мог решить, что вперёд идти некуда: координата
                    // назад не откатывалась. Будим его, чтобы он взял блок этого
                    // курсора, даже если он остался позади.
                    waited = true;
                    self.cache.inner.wake.notify_all();
                }
                state = self.cache.inner.wait_timeout(state);
            }
        };

        if waited {
            let elapsed = started.elapsed();
            if elapsed >= SLOW_READ {
                self.cache.inner.counters.add_slow_read(elapsed);
            }
        }
        if matches!(&outcome, Ok(read) if *read > 0) {
            // Курсор сдвинулся: загрузчику могло освободиться место, а
            // при нулевом read-ahead именно это и разрешает следующий блок.
            self.cache.inner.wake.notify_all();
        }
        outcome
    }
}

impl Seek for CacheCursor {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "открытие отменено",
            ));
        }

        let current = self.position.load(Ordering::SeqCst);
        let target = match position {
            SeekFrom::Start(offset) => i128::from(offset),
            SeekFrom::Current(delta) => i128::from(current) + i128::from(delta),
            SeekFrom::End(delta) => {
                let Some(len) = self.cache.inner.len else {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        "размер источника неизвестен",
                    ));
                };
                i128::from(len) + i128::from(delta)
            }
        };

        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "позиция до начала источника",
            ));
        }
        let target = target as u64;
        if let Some(len) = self.cache.inner.len
            && target > len
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "позиция за концом источника",
            ));
        }

        let mut state = self.cache.inner.lock();
        if state.closed || state.error.is_some() {
            // Источник уже не оживёт; причину вернёт `read`.
            self.position.store(target, Ordering::SeqCst);
            return Ok(target);
        }
        state.demand = true;

        let ready = locate(&state.blocks, target).is_some()
            || self.cache.inner.len.is_some_and(|len| target >= len)
            || state.eof_at.is_some_and(|eof| target >= eof);
        let block = self.cache.inner.config.block as u64;
        let target_block = align_down(target, block);
        let backward = target_block < state.fetch_pos;
        if (!ready && state.fetch_pos != target_block) || backward {
            // Блоки не выбрасываем: демуксер MP4 прыгает в конец за `moov`
            // и обратно, и после возврата данные должны найтись в кэше.
            // Прежний запрос прерывается, но уже прочитанные байты кэш
            // сохранит. Возврат назад отменяет и read-ahead вперёд.
            state.abort.store(true, Ordering::SeqCst);
            state.abort = Arc::new(AtomicBool::new(false));
        }
        if backward {
            // После возврата из конца файла загрузчик должен снова идти от
            // курсора, иначе он останется далеко впереди и не будет читать
            // вперёд.
            state.fetch_pos = target_block;
        }

        self.position.store(target, Ordering::SeqCst);
        drop(state);
        self.cache.inner.wake.notify_all();
        Ok(target)
    }
}

/// Фоновой загрузчик: идёт по источнику вперёд, вытесняет за курсорами.
fn run_loader(inner: Arc<Inner>) {
    let mut state = inner.lock();
    loop {
        if state.closed || state.error.is_some() {
            return;
        }
        if state.cursors.is_empty() || !state.demand {
            state = inner.wait(state);
            continue;
        }

        let positions: Vec<u64> = state
            .cursors
            .values()
            .map(|position| position.load(Ordering::SeqCst))
            .collect();
        let max_pos = positions.iter().copied().max().unwrap_or(0);

        // Ближайший блок, которого ждёт какой-то курсор, важнее чтения вперёд.
        // Иначе при пробе демуксера (уход в конец файла) второй курсор в начале
        // ждал бы блок, который загрузчик уже прошёл.
        if let Some(pos) = positions
            .iter()
            .copied()
            .filter(|pos| locate(&state.blocks, *pos).is_none())
            .min()
        {
            state.fetch_pos = align_down(pos, inner.config.block as u64);
        }
        // Не качаем то, что уже лежит в кэше: после приоритетного блока или
        // перемотки одного курсора вперёд второй мог уже заполнить участок.
        while let Some((_, end)) = locate(&state.blocks, state.fetch_pos) {
            state.fetch_pos = end;
        }

        let at_end = state.eof_at.is_some() || inner.len.is_some_and(|len| state.fetch_pos >= len);
        if at_end || state.fetch_pos > max_pos.saturating_add(inner.config.read_ahead) {
            state = inner.wait(state);
            continue;
        }

        let want = fetch_len(inner.config.block, inner.len, state.fetch_pos);
        if want == 0 {
            state = inner.wait(state);
            continue;
        }
        if !make_space(inner.config.capacity, &mut state, want) {
            state = inner.wait(state);
            continue;
        }

        let offset = state.fetch_pos;
        let abort = Arc::clone(&state.abort);
        drop(state);

        inner.counters.add_request();
        let outcome = (inner.loader)(offset, want, &abort);
        state = inner.lock();

        if state.closed {
            return;
        }
        // Перемотка: загрузчику пора к другой позиции, но всё, что успело
        // прийти, — валидные байты источника, поэтому сохраняем их, а не
        // выбрасываем: иначе каждая перемотка теряет до целого блока.
        let stale = abort.load(Ordering::SeqCst);

        match outcome {
            Ok(data) if data.is_empty() => {
                if stale {
                    continue;
                }
                state.eof_at = Some(offset);
                drop(state);
                inner.wake.notify_all();
                state = inner.lock();
            }
            Ok(mut data) => {
                data.truncate(want);
                inner.counters.add_bytes(data.len());
                state.bytes += data.len();
                // При отмене `fetch_pos` уже указывает на цель перемотки:
                // не откатываем его назад к старой позиции.
                if !stale {
                    state.fetch_pos = offset + data.len() as u64;
                }
                state.blocks.push_back(Block {
                    start: offset,
                    data,
                });
                drop(state);
                inner.wake.notify_all();
                state = inner.lock();
            }
            Err(error) if error.cancelled => {
                if stale {
                    continue;
                }
                // Если это не перемотка, значит закрывают кэш: ждём `Drop`.
                state = inner.wait(state);
            }
            Err(error) => {
                if stale {
                    // Обрыв, вызванный собственной отменой, не должен
                    // становиться терминальной ошибкой источника.
                    continue;
                }
                state.error = Some(error.message);
                drop(state);
                inner.wake.notify_all();
                return;
            }
        }
    }
}

/// Сколько байт просить у загрузчика с этой позиции.
fn fetch_len(block: usize, len: Option<u64>, position: u64) -> usize {
    match len {
        Some(len) => len.saturating_sub(position).min(block as u64) as usize,
        None => block,
    }
}

/// Освобождает место, вытесняя только блоки позади всех курсоров.
///
/// `false` — места нет и вытеснять нечего: загрузчик должен ждать, пока
/// медленный курсор не сдвинется (backpressure).
fn make_space(capacity: usize, state: &mut State, need: usize) -> bool {
    let Some(min_pos) = state
        .cursors
        .values()
        .map(|position| position.load(Ordering::SeqCst))
        .min()
    else {
        return false;
    };

    while state.bytes.saturating_add(need) > capacity {
        // После перемотки блоки лежат не по порядку, поэтому ищем любой
        // блок целиком позади всех курсоров, а не только самый старый.
        let Some(index) = state.blocks.iter().position(|block| block.end() <= min_pos) else {
            if state.blocks.is_empty() {
                return need <= capacity;
            }
            // Курсор стоит в дыре: нужного блока нет, а в кэше только блоки
            // впереди. Строгое правило их не отдаёт, но без этого загрузчик
            // не сможет докачать пропущенное и чтение встанет навсегда.
            // Жертвуем самым дальним блоком — под курсором его всё равно нет.
            let in_hole = !state
                .blocks
                .iter()
                .any(|block| block.start <= min_pos && min_pos < block.end());
            if !in_hole {
                return false;
            }
            let index = state
                .blocks
                .iter()
                .enumerate()
                .filter(|(_, block)| {
                    !state.cursors.values().any(|position| {
                        let position = position.load(Ordering::SeqCst);
                        block.start <= position && position < block.end()
                    })
                })
                .max_by_key(|(_, block)| block.start)
                .map(|(index, _)| index)
                .or_else(|| {
                    // Все блоки заняты курсорами: без вытеснения не обойтись,
                    // иначе медленный курсор в дыре не дождётся данных.
                    state
                        .blocks
                        .iter()
                        .enumerate()
                        .max_by_key(|(_, block)| block.start)
                        .map(|(index, _)| index)
                })
                .expect("кэш не пуст");
            let block = state.blocks.remove(index).expect("индекс проверен");
            state.bytes -= block.data.len();
            continue;
        };
        let block = state.blocks.remove(index).expect("блок уже проверен");
        state.bytes -= block.data.len();
    }
    true
}

/// Находит блок, содержащий позицию: `(индекс, конец блока)`.
fn locate(blocks: &VecDeque<Block>, position: u64) -> Option<(usize, u64)> {
    blocks
        .iter()
        .position(|block| position >= block.start && position < block.end())
        .map(|index| (index, blocks[index].end()))
}

/// Начало блока, содержащего позицию.
fn align_down(value: u64, block: u64) -> u64 {
    value - value % block
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    const BLOCK: usize = 64;

    /// Журнал запросов загрузчика: смещение и длина.
    type Calls = Arc<Mutex<Vec<(u64, usize)>>>;

    fn sample(len: usize) -> Vec<u8> {
        (0..len).map(|index| (index % 251) as u8).collect()
    }

    fn config(capacity_blocks: usize, read_ahead_blocks: u64) -> CacheConfig {
        CacheConfig {
            block: BLOCK,
            capacity: capacity_blocks * BLOCK,
            read_ahead: read_ahead_blocks * BLOCK as u64,
        }
    }

    fn no_cancel() -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(false))
    }

    fn counters() -> Arc<CacheCounters> {
        Arc::new(CacheCounters::default())
    }

    fn counting_loader(
        data: Arc<Vec<u8>>,
        calls: Calls,
    ) -> impl Fn(u64, usize, &AtomicBool) -> Result<Vec<u8>, FetchError> + Send + Sync + 'static
    {
        move |offset, len, _abort| {
            calls.lock().unwrap().push((offset, len));
            let start = usize::try_from(offset).expect("смещение в usize");
            if start >= data.len() {
                return Ok(Vec::new());
            }
            let end = (start + len).min(data.len());
            Ok(data[start..end].to_vec())
        }
    }

    fn cache_of(bytes: &[u8], config: CacheConfig) -> (Arc<Cache>, Calls, Arc<Vec<u8>>) {
        let data = Arc::new(bytes.to_vec());
        let calls = Arc::new(Mutex::new(Vec::new()));
        let cache = Cache::start(
            config,
            Some(data.len() as u64),
            counters(),
            counting_loader(Arc::clone(&data), Arc::clone(&calls)),
        )
        .expect("открыть кэш");
        (cache, calls, data)
    }

    #[test]
    fn sequential_read_returns_the_whole_source() {
        let source = sample(3 * BLOCK + BLOCK / 2);
        let (cache, calls, data) = cache_of(&source, config(8, 8));
        let mut cursor = cache.cursor(no_cancel());

        let mut read = Vec::new();
        cursor.read_to_end(&mut read).expect("прочитать источник");

        assert_eq!(read.as_slice(), data.as_slice());
        assert_eq!(calls.lock().unwrap().len(), 4);
        assert_eq!(cache.len(), Some(data.len() as u64));
        let stats = cache.network_stats();
        assert_eq!(stats.requests, 4);
        assert_eq!(stats.bytes, data.len() as u64);
    }

    #[test]
    fn two_cursors_share_one_download() {
        let source = sample(4 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(8, 8));
        let mut video = cache.cursor(no_cancel());

        let mut first = Vec::new();
        video
            .read_to_end(&mut first)
            .expect("видео прочитало источник");
        assert_eq!(first.as_slice(), data.as_slice());

        let mut audio = cache.cursor(no_cancel());
        let mut second = Vec::new();
        audio.read_to_end(&mut second).expect("звук читает из кэша");
        assert_eq!(second.as_slice(), data.as_slice());
        assert_eq!(calls.lock().unwrap().len(), 4, "файл скачан один раз");
    }

    #[test]
    fn seek_back_inside_cache_is_free() {
        let source = sample(2 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(4, 0));
        let mut cursor = cache.cursor(no_cancel());

        let mut head = vec![0; 2 * BLOCK];
        cursor.read_exact(&mut head).expect("прочитать источник");
        assert_eq!(calls.lock().unwrap().len(), 2);

        cursor
            .seek(SeekFrom::Start(BLOCK as u64 / 2))
            .expect("перемотка внутри кэша");
        let mut byte = [0_u8; 1];
        cursor
            .read_exact(&mut byte)
            .expect("прочитать после перемотки");

        assert_eq!(byte[0], data[BLOCK / 2]);
        assert_eq!(calls.lock().unwrap().len(), 2, "кэш не перезапрашивали");
    }

    #[test]
    fn seek_outside_cache_refetches_from_the_target() {
        let source = sample(16 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(4, 0));
        let mut cursor = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        cursor.read_exact(&mut byte).expect("прочитать начало");

        let target = 10 * BLOCK as u64;
        cursor
            .seek(SeekFrom::Start(target))
            .expect("перемотка вперёд");
        cursor
            .read_exact(&mut byte)
            .expect("прочитать после перемотки");

        assert_eq!(byte[0], data[10 * BLOCK]);
        assert_eq!(cursor.position(), target + 1);
        assert_eq!(
            calls.lock().unwrap().last().map(|(offset, _)| *offset),
            Some(target),
            "загрузка с целевого блока"
        );
        assert_eq!(
            cache.buffered_bytes(),
            2 * BLOCK,
            "начало остаётся в кэше для возврата демуксера"
        );
    }

    #[test]
    fn seek_back_to_a_kept_block_is_free() {
        let source = sample(16 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(16, 0));
        let mut cursor = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        cursor.read_exact(&mut byte).expect("прочитать начало");
        cursor
            .seek(SeekFrom::Start(10 * BLOCK as u64))
            .expect("перемотка вперёд");
        cursor
            .read_exact(&mut byte)
            .expect("прочитать после перемотки");
        assert_eq!(byte[0], data[10 * BLOCK]);
        let requests = calls.lock().unwrap().len();

        cursor.seek(SeekFrom::Start(0)).expect("перемотка назад");
        cursor.read_exact(&mut byte).expect("начало из кэша");

        assert_eq!(byte[0], data[0]);
        assert_eq!(
            calls.lock().unwrap().len(),
            requests,
            "ранее скачанный блок не запрашивается повторно"
        );
    }

    #[test]
    fn eviction_handles_out_of_order_blocks() {
        // Сначала блок в конце, потом возврат назад: блоки лежат не по
        // порядку. Вытеснять нужно не только самый старый, иначе загрузчик
        // ждёт место, которого не дождётся, и чтение встаёт навсегда.
        let source = sample(16 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(2, 0));
        let mut cursor = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        cursor
            .seek(SeekFrom::Start(10 * BLOCK as u64))
            .expect("перемотка в конец");
        cursor.read_exact(&mut byte).expect("дальний блок");
        assert_eq!(byte[0], data[10 * BLOCK]);

        cursor
            .seek(SeekFrom::Start(BLOCK as u64))
            .expect("перемотка назад");

        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut rest = vec![0_u8; 2 * BLOCK];
            let result = cursor.read_exact(&mut rest);
            done_tx.send((result.is_ok(), rest)).ok();
        });

        let (ok, rest) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("вытеснение не должно блокировать чтение");
        assert!(ok, "чтение после вытеснения");
        assert_eq!(rest.as_slice(), &data[BLOCK..3 * BLOCK]);
        reader.join().expect("поток читателя");
    }

    #[test]
    fn both_cursors_survive_a_seek_back() {
        let source = sample(32 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(32, 4));
        let mut audio = cache.cursor(no_cancel());
        let mut video = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        audio.read_exact(&mut byte).expect("звук начал");
        video.read_exact(&mut byte).expect("видео начало");

        // Видео ушло вперёд и вернулось, пока звук читает с начала.
        video
            .seek(SeekFrom::Start(20 * BLOCK as u64))
            .expect("видео вперёд");
        video.read_exact(&mut byte).expect("дальний блок");
        assert_eq!(byte[0], data[20 * BLOCK]);
        video
            .seek(SeekFrom::Start(2 * BLOCK as u64))
            .expect("видео назад");

        let (audio_tx, audio_rx) = mpsc::channel();
        let audio_reader = thread::spawn(move || {
            let mut read = Vec::new();
            let result = audio.read_to_end(&mut read);
            audio_tx.send((result.is_ok(), read)).ok();
        });
        let (video_tx, video_rx) = mpsc::channel();
        let video_reader = thread::spawn(move || {
            let mut read = Vec::new();
            let result = video.read_to_end(&mut read);
            video_tx.send((result.is_ok(), read)).ok();
        });

        let (audio_ok, audio_read) = audio_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("звук дочитал источник");
        let (video_ok, video_read) = video_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("видео дочитало источник");

        assert!(audio_ok && video_ok);
        assert_eq!(audio_read.as_slice(), &data[1..]);
        assert_eq!(video_read.as_slice(), &data[2 * BLOCK..]);
        audio_reader.join().expect("поток звука");
        video_reader.join().expect("поток видео");
    }

    #[test]
    fn a_far_seek_does_not_starve_the_second_cursor() {
        // Проба MP4 уходит в конец файла за moov, пока звук ждёт начало:
        // загрузчик не должен идти только вперёд от уехавшего курсора.
        let source = sample(64 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(64, 8));
        let mut audio = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        audio.read_exact(&mut byte).expect("звук прочитал начало");

        let mut video = cache.cursor(no_cancel());
        let far = 50 * BLOCK as u64;
        video
            .seek(SeekFrom::Start(far))
            .expect("видео ушло в конец");
        let mut tail = vec![0; BLOCK];
        video.read_exact(&mut tail).expect("видео прочитало конец");
        assert_eq!(tail.as_slice(), &data[50 * BLOCK..51 * BLOCK]);

        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut read = Vec::new();
            let result = audio.read_to_end(&mut read);
            done_tx.send((result.is_ok(), read)).ok();
        });

        let (ok, read) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("звук не должен ждать блок, который загрузчик прошёл");
        assert!(ok);
        assert_eq!(read.as_slice(), &data[1..]);
        reader.join().expect("поток звука");
    }

    #[test]
    fn cache_reports_resident_bytes() {
        let source = sample(4 * BLOCK);
        let (cache, _calls, _data) = cache_of(&source, config(2, 0));
        let mut cursor = cache.cursor(no_cancel());

        let mut byte = [0_u8; 1];
        cursor.read_exact(&mut byte).expect("прочитать начало");
        assert_eq!(
            cache.network_stats().cache_bytes,
            BLOCK as u64,
            "в кэше ровно один блок"
        );

        let mut rest = vec![0_u8; 3 * BLOCK - 1];
        cursor.read_exact(&mut rest).expect("прочитать остальное");

        // Загрузчик асинхронный: после чтения последний блок может ещё
        // докачиваться, а вытеснение уже освободило предыдущий.
        let deadline = Instant::now() + Duration::from_secs(2);
        while cache.network_stats().cache_bytes != 2 * BLOCK as u64 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            cache.network_stats().cache_bytes,
            2 * BLOCK as u64,
            "занятость не превышает capacity"
        );
    }

    #[test]
    fn eviction_keeps_memory_within_capacity() {
        let source = sample(8 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(3, 8));
        let mut cursor = cache.cursor(no_cancel());

        let mut read = Vec::new();
        cursor.read_to_end(&mut read).expect("прочитать источник");

        assert_eq!(read.as_slice(), data.as_slice());
        assert_eq!(calls.lock().unwrap().len(), 8);
        assert!(
            cache.buffered_bytes() <= 3 * BLOCK,
            "кэш не вырос сверх лимита: {} > {}",
            cache.buffered_bytes(),
            3 * BLOCK
        );
    }

    #[test]
    fn seek_back_after_eviction_downloads_again() {
        let source = sample(8 * BLOCK);
        let (cache, calls, data) = cache_of(&source, config(2, 8));
        let mut cursor = cache.cursor(no_cancel());

        let mut read = Vec::new();
        cursor.read_to_end(&mut read).expect("прочитать источник");
        let before = calls.lock().unwrap().len();

        cursor.seek(SeekFrom::Start(0)).expect("перемотка в начало");
        let mut byte = [0_u8; 1];
        cursor.read_exact(&mut byte).expect("прочитать начало");

        assert_eq!(byte[0], data[0]);
        assert!(
            calls.lock().unwrap().len() > before,
            "вытесненные блоки качаются заново"
        );
    }

    #[test]
    fn backpressure_waits_for_the_slow_cursor() {
        let source = sample(4 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(2, 8));
        let mut slow = cache.cursor(no_cancel());

        // Медленный курсор остановился в середине первого блока: вытеснять
        // этот блок нельзя, значит загрузчику негде взять место.
        let mut half = vec![0; BLOCK / 2];
        slow.read_exact(&mut half)
            .expect("прочитать половину блока");
        assert_eq!(half.as_slice(), &data[..BLOCK / 2]);

        let fast_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let fast = thread::spawn(move || {
            let mut cursor = fast_cache.cursor(no_cancel());
            let mut read = Vec::new();
            let result = cursor.read_to_end(&mut read);
            done_tx.send((result.is_ok(), read)).ok();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "кэш не должен рвать чтение"
        );

        let mut slow_rest = Vec::new();
        slow.read_to_end(&mut slow_rest)
            .expect("медленный курсор дочитал");

        let (ok, read) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("быстрый курсор дождался места");
        assert!(ok);
        assert_eq!(read.as_slice(), data.as_slice());
        assert_eq!(slow_rest.as_slice(), &data[BLOCK / 2..]);

        fast.join().expect("поток быстрого курсора");
    }

    #[test]
    fn dropping_a_cursor_releases_the_loaded_blocks() {
        let source = sample(4 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(2, 8));
        let mut slow = cache.cursor(no_cancel());

        let mut half = vec![0; BLOCK / 2];
        slow.read_exact(&mut half)
            .expect("прочитать половину блока");

        let fast_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let fast = thread::spawn(move || {
            let mut cursor = fast_cache.cursor(no_cancel());
            let mut read = Vec::new();
            let result = cursor.read_to_end(&mut read);
            done_tx.send((result.is_ok(), read)).ok();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "быстрый курсор ждёт места"
        );

        drop(slow);

        let (ok, read) = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("место освободилось");
        assert!(ok);
        assert_eq!(read.as_slice(), data.as_slice());
        fast.join().expect("поток быстрого курсора");
    }

    #[test]
    fn loader_error_wakes_both_cursors() {
        let source = sample(4 * BLOCK);
        let data = Arc::new(source);
        let calls = Arc::new(AtomicU64::new(0));
        let loader_data = Arc::clone(&data);
        let loader_calls = Arc::clone(&calls);
        let cache = Cache::start(
            config(8, 0),
            Some(data.len() as u64),
            counters(),
            move |offset, len, _| {
                let call = loader_calls.fetch_add(1, Ordering::SeqCst);
                if call >= 1 {
                    return Err(FetchError::other("boom"));
                }
                let start = usize::try_from(offset).expect("смещение в usize");
                let end = (start + len).min(loader_data.len());
                Ok(loader_data[start..end].to_vec())
            },
        )
        .expect("открыть кэш");

        let mut first = cache.cursor(no_cancel());
        let mut head = vec![0; BLOCK];
        first.read_exact(&mut head).expect("первый блок в порядке");
        let error = first.read(&mut head).expect_err("загрузчик сломался");
        assert!(error.to_string().contains("boom"));

        let mut second = cache.cursor(no_cancel());
        let error = second
            .read(&mut head)
            .expect_err("ошибка общая на весь источник");
        assert!(error.to_string().contains("boom"));
    }

    #[test]
    fn close_wakes_a_waiting_cursor() {
        let cache = Cache::start(
            config(2, 2),
            Some(4 * BLOCK as u64),
            counters(),
            |_offset, _len, abort| {
                while !abort.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(FetchError::cancelled())
            },
        )
        .expect("открыть кэш");

        let reader_cache = Arc::clone(&cache);
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut cursor = reader_cache.cursor(no_cancel());
            let mut buffer = [0_u8; 16];
            let result = cursor.read(&mut buffer);
            done_tx.send(result.map_err(|error| error.kind())).ok();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "курсор ждёт данные"
        );

        cache.close();

        let kind = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("курсор разбужен")
            .expect_err("чтение остановлено");
        assert_eq!(kind, io::ErrorKind::Interrupted);
        reader.join().expect("поток читателя");
    }

    #[test]
    fn cancel_wakes_a_waiting_cursor() {
        let cancel = Arc::new(AtomicBool::new(false));
        let cache = Cache::start(
            config(2, 2),
            Some(4 * BLOCK as u64),
            counters(),
            |_offset, _len, abort| {
                while !abort.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(1));
                }
                Err(FetchError::cancelled())
            },
        )
        .expect("открыть кэш");

        let reader_cache = Arc::clone(&cache);
        let reader_cancel = Arc::clone(&cancel);
        let (done_tx, done_rx) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut cursor = reader_cache.cursor(reader_cancel);
            let mut buffer = [0_u8; 16];
            let result = cursor.read(&mut buffer);
            done_tx.send(result.map_err(|error| error.kind())).ok();
        });

        assert!(
            done_rx.recv_timeout(Duration::from_millis(150)).is_err(),
            "курсор ждёт данные"
        );

        cancel.store(true, Ordering::SeqCst);

        let kind = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("курсор разбужен отменой")
            .expect_err("чтение отменено");
        assert_eq!(kind, io::ErrorKind::Interrupted);
        reader.join().expect("поток читателя");
    }

    #[test]
    fn read_past_the_end_returns_zero() {
        let source = sample(2 * BLOCK + 7);
        let (cache, _calls, data) = cache_of(&source, config(4, 1));
        let mut cursor = cache.cursor(no_cancel());

        let mut read = Vec::new();
        cursor.read_to_end(&mut read).expect("прочитать источник");
        assert_eq!(read.as_slice(), data.as_slice());

        let mut extra = [0_u8; 8];
        assert_eq!(cursor.read(&mut extra).expect("конец источника"), 0);
    }

    #[test]
    fn seek_from_end_uses_the_known_length() {
        let source = sample(3 * BLOCK);
        let (cache, _calls, data) = cache_of(&source, config(4, 0));
        let mut cursor = cache.cursor(no_cancel());

        let target = cursor.seek(SeekFrom::End(-25)).expect("перемотка от конца");
        assert_eq!(target, 3 * BLOCK as u64 - 25);

        let mut tail = Vec::new();
        cursor.read_to_end(&mut tail).expect("прочитать хвост");
        assert_eq!(tail.as_slice(), &data[3 * BLOCK - 25..]);
    }

    #[test]
    fn seek_beyond_the_end_is_rejected() {
        let source = sample(2 * BLOCK);
        let (cache, _calls, _data) = cache_of(&source, config(2, 0));
        let mut cursor = cache.cursor(no_cancel());

        let error = cursor
            .seek(SeekFrom::Start(2 * BLOCK as u64 + 1))
            .expect_err("позиция за концом");
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn unknown_length_streams_until_the_loader_says_end() {
        let source = sample(2 * BLOCK + BLOCK / 2);
        let data = Arc::new(source);
        let loader_data = Arc::clone(&data);
        let cache = Cache::start(config(8, 8), None, counters(), move |offset, len, _| {
            let start = usize::try_from(offset).expect("смещение в usize");
            if start >= loader_data.len() {
                return Ok(Vec::new());
            }
            let end = (start + len).min(loader_data.len());
            Ok(loader_data[start..end].to_vec())
        })
        .expect("открыть кэш");

        let mut cursor = cache.cursor(no_cancel());
        let mut read = Vec::new();
        cursor.read_to_end(&mut read).expect("прочитать поток");

        assert_eq!(read.as_slice(), data.as_slice());
        assert_eq!(cache.len(), None);

        let mut extra = [0_u8; 4];
        assert_eq!(cursor.read(&mut extra).expect("конец потока"), 0);
    }

    #[test]
    fn seek_from_end_without_length_is_unsupported() {
        let cache = Cache::start(config(2, 0), None, counters(), |_offset, _len, _| {
            Ok(Vec::new())
        })
        .expect("открыть кэш");
        let mut cursor = cache.cursor(no_cancel());

        let error = cursor.seek(SeekFrom::End(0)).expect_err("длина неизвестна");
        assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    }
}
