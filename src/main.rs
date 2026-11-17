// ============================================================================
//  LINESORT — сортировщик и очистка больших текстовых файлов
// ============================================================================
//
//  Утилита командной строки для построчной обработки текстовых файлов
//  (TXT, CSV, TSV, LOG и любых других построчных форматов):
//
//    1. Удаление дубликатов строк (включая файлы, не помещающиеся в RAM).
//    2. Сортировка: по алфавиту (байты Unicode/UTF-8) или естественная
//       (числа сравниваются как числа: "2" < "10"), по возрастанию/убыванию.
//    3. Обрезка слишком длинных строк до N СИМВОЛОВ (не байтов).
//    4. Разбивка выходного файла на части (по строкам и/или по объёму).
//    5. Обработка каталога: каждый файл отдельно, либо --merge — всё в один.
//    6. Информативный прогресс: фазы, бар, скорость, ETA, итоговая статистика.
//
//  ГЛАВНОЕ СВОЙСТВО: память ограничена бюджетом (--memory, по умолчанию
//  512 МиБ) НЕЗАВИСИМО от размера входа. Реализована ВНЕШНЯЯ СОРТИРОВКА
//  со слиянием (external merge sort):
//
//      вход ──чтение чанками──> арена в памяти ──(бюджет исчерпан)──>
//        параллельная сортировка долей ──> «серия» (run) во врем. файл
//      ...повтор...  ──> k-сторонее слияние всех серий (heap) ──> выход
//
//  Оптимизации:
//    * SIMD-поиск переводов строк (AVX2/SSE2, автоопределение; скалярный
//      запасной вариант для других архитектур);
//    * аренные строки (bump-аллокатор): одна большая Vec<u8> на серию,
//      сортируются только 16-байтовые ссылки {offset,len} — нулевые
//      накладные расходы на String;
//    * параллельная сортировка долей серии (--jobs, std::thread::scope)
//      с последующим ленивым слиянием при записи серии;
//    * многопроходное слияние, если серий больше лимита открытых файлов,
//      группы сливаются параллельно;
//    * дедупликация «по соседству» после полной сортировки — не требует
//      хеш-таблиц в памяти (корректно при любом объёме данных);
//    * конвертация кодировок cp1251 / koi8-r / ibm866 -> UTF-8 по
//      статическим таблицам (вход всегда нормализуется в UTF-8);
//    * вся запись идёт через временный файл с атомарным rename — на диске
//      не остаётся недописанных результатов.
//
//  Режим --no-sort (сохранить исходный порядок, но убрать дубликаты)
//  реализован двумя прогонами внешней сортировки по служебному ключу
//  (номер строки): сначала группировка одинаковых строк с сохранением
//  минимального номера, затем упорядочивание уникальных по номеру.
//
//  Только стандартная библиотека (std), никаких внешних крейтов.
//  Одиночный файл: `rustc -O linesort.rs -o linesort` собирает утилиту.
//
//  Структура исходника (по порядку):
//    misc      — форматирование чисел/размеров/времени, тип ошибок
//    progress  — прогресс-бар, тикер, итоговая статистика
//    encoding  — таблицы cp1251/koi8-r/ibm866 и конвертер в UTF-8
//    simd      — быстрый поиск '\n' (memchr)
//    io        — построчный читатель чанками, многофайловый reader,
//                писатель с разбивкой на части и атомарным rename
//    arena     — арена строк и ссылки LineRef
//    cmp       — компараторы (байтовый, естественный, регистронезависимый)
//    chunksort — параллельная сортировка долей + heap-слияние долей
//    extsort   — внешняя сортировка: серии, k-way merge, многопроходность
//    app       — конвейер задачи, каталоги, имена выходов, отчёты
//    cli       — разбор аргументов, справка --help
//    main      — входная точка
// ============================================================================

use std::cmp::Ordering::{self, Equal, Greater, Less};
use std::collections::BinaryHeap;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, BufWriter, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{Duration, Instant};

// ============================================================================
//  misc — мелкие утилиты форматирования и тип ошибки приложения
// ============================================================================

/// Ошибка приложения: сохраняет человекочитаемое сообщение на русском.
/// exit-код 1 — ошибка использования, 2 — ошибка ввода-вывода/данных.
#[derive(Debug)]
pub enum AppError {
    Usage(String),
    Io(String),
}

impl fmt::Display for AppError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppError::Usage(m) => write!(f, "ошибка аргументов: {m}"),
            AppError::Io(m) => write!(f, "ошибка: {m}"),
        }
    }
}

impl From<io::Error> for AppError {
    fn from(e: io::Error) -> Self {
        AppError::Io(e.to_string())
    }
}

type Res<T> = Result<T, AppError>;

/// 1234567 -> "1 234 567" (тонкие пробелы как разделители тысяч).
pub fn fmt_thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    let bytes = s.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(' ');
        }
        out.push(b as char);
    }
    out
}

/// Человекочитаемый размер: 512 Б / 1,5 КиБ / 3,2 ГиБ (двоичные единицы).
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["Б", "КиБ", "МиБ", "ГиБ", "ТиБ", "ПиБ"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} Б")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

/// Длительность в виде Ч:ММ:СС (или ММ:СС, если меньше часа).
pub fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

/// Разбор размера с суффиксом: "512", "512K", "100M", "2G", "1G64M".
/// Регистр не важен; без суффикса — байты. K/M/G = 1024-кратные.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().replace(' ', "");
    if s.is_empty() {
        return None;
    }
    let mut num = String::new();
    let mut total: u64 = 0;
    let mut any = false;
    for ch in s.chars() {
        if ch.is_ascii_digit() {
            num.push(ch);
        } else {
            let mult = match ch.to_ascii_uppercase() {
                'K' => 1024u64,
                'M' => 1024 * 1024,
                'G' => 1024 * 1024 * 1024,
                'T' => 1024u64 * 1024 * 1024 * 1024,
                _ => return None,
            };
            let n: u64 = num.parse().ok()?;
            total = total.checked_add(n.checked_mul(mult)?)?;
            num.clear();
            any = true;
        }
    }
    if !num.is_empty() {
        let n: u64 = num.parse().ok()?;
        total = total.checked_add(n)?;
        any = true;
    }
    if any {
        Some(total)
    } else {
        None
    }
}

// ============================================================================
//  progress — информативное отображение хода выполнения
// ============================================================================
//  Прогресс и все служебные сообщения пишутся в stderr: stdout остаётся
//  чистым для данных (полезно при --output -). Если stderr — терминал,
//  рисуется однострочный бар, обновляемый не чаще 100 мс; иначе (редирект
//  в файл) — компактные строки раз в 10 секунд, чтобы лог был читаем.

pub struct Progress {
    /// полный режим (по умолчанию); --quiet оставляет только итог и ошибки
    pub enabled: bool,
    /// --verbose: подробные сообщения о каждой серии/проходе
    pub verbose: bool,
    /// stderr — терминал: можно рисовать \r-бар
    tty: bool,
    /// общая длительность работы утилиты
    pub start: Instant,
    /// ожидаемый объём чтения в текущей фазе (байты), если известен
    pub total_bytes: AtomicU64,
    /// прочитано байт (обновляется из потоков слияния)
    pub bytes: AtomicU64,
    /// обработано строк
    pub lines: AtomicU64,
    /// произвольный счётчик фазы (например, готово групп из N)
    pub done: AtomicU64,
    pub goal: AtomicU64,
    /// подпись фазы: "чтение и сортировка", "слияние", ...
    phase: std::sync::Mutex<String>,
    last_paint: std::sync::Mutex<Instant>,
    /// момент старта текущей фазы (для скорости/ETA именно этой фазы)
    phase_start: std::sync::Mutex<Instant>,
}

impl Progress {
    pub fn new(enabled: bool, verbose: bool) -> Self {
        Progress {
            enabled,
            verbose,
            tty: io::stderr().is_terminal(),
            start: Instant::now(),
            total_bytes: AtomicU64::new(0),
            bytes: AtomicU64::new(0),
            lines: AtomicU64::new(0),
            done: AtomicU64::new(0),
            goal: AtomicU64::new(0),
            phase: std::sync::Mutex::new(String::new()),
            last_paint: std::sync::Mutex::new(Instant::now() - Duration::from_secs(1)),
            phase_start: std::sync::Mutex::new(Instant::now()),
        }
    }

    /// Начать фазу: подпись + ожидаемый объём работы (байты или счётчик).
    pub fn phase(&self, name: &str, total_bytes: u64, goal: u64) {
        *self.phase.lock().unwrap() = name.to_string();
        self.total_bytes.store(total_bytes, AtomicOrdering::Relaxed);
        self.goal.store(goal, AtomicOrdering::Relaxed);
        self.bytes.store(0, AtomicOrdering::Relaxed);
        self.lines.store(0, AtomicOrdering::Relaxed);
        self.done.store(0, AtomicOrdering::Relaxed);
        *self.last_paint.lock().unwrap() = Instant::now() - Duration::from_secs(1);
        *self.phase_start.lock().unwrap() = Instant::now();
        if self.enabled {
            eprintln!("==> {name}");
        }
    }

    /// Обычное сообщение (без --quiet).
    pub fn msg(&self, s: &str) {
        if self.enabled {
            self.end_line();
            eprintln!("{s}");
        }
    }

    /// Подробное сообщение (только с --verbose).
    pub fn vmsg(&self, s: &str) {
        if self.enabled && self.verbose {
            self.end_line();
            eprintln!("    {s}");
        }
    }

    /// Завершить строку бара переводом строки (перед обычным выводом).
    pub fn end_line(&self) {
        if self.tty {
            // Бар рисуется через \r в одну строку; перед новым сообщением
            // её нужно закрыть, чтобы текст не «съехал».
            let painted = self.last_paint.lock().unwrap();
            if painted.elapsed() < Duration::from_millis(50) {
                eprintln!();
            }
        }
    }

    /// Перерисовать бар, если прошло >= 100 мс с прошлой отрисовки.
    /// Вызывается из горячих циклов; Instant::now() дёшев (vDSO).
    pub fn tick(&self) {
        if !self.enabled || !self.tty {
            return;
        }
        let mut last = self.last_paint.lock().unwrap();
        if last.elapsed() < Duration::from_millis(100) {
            return;
        }
        *last = Instant::now();
        drop(last);
        self.paint();
    }

    /// Нарисовать бар (только при tty).
    fn paint(&self) {
        let total = self.total_bytes.load(AtomicOrdering::Relaxed);
        let bytes = self.bytes.load(AtomicOrdering::Relaxed);
        let lines = self.lines.load(AtomicOrdering::Relaxed);
        let phase = self.phase.lock().unwrap().clone();

        // Доля выполненного: по байтам, иначе по счётчику done/goal.
        let pct = if total > 0 {
            (bytes as f64 / total as f64).clamp(0.0, 1.0)
        } else {
            let g = self.goal.load(AtomicOrdering::Relaxed);
            if g > 0 {
                let d = self.done.load(AtomicOrdering::Relaxed);
                (d as f64 / g as f64).clamp(0.0, 1.0)
            } else {
                0.0
            }
        };

        // Скорость и ETA — по байтам текущей фазы и времени фазы.
        let bar_w = 22usize;
        let filled = (pct * bar_w as f64).round() as usize;
        let mut bar = String::with_capacity(bar_w + 2);
        bar.push('[');
        for i in 0..bar_w {
            bar.push(if i < filled { '=' } else { '.' });
        }
        bar.push(']');

        let phase_elapsed = self.phase_start.lock().unwrap().elapsed().as_secs_f64().max(1e-9);
        let speed = bytes as f64 / phase_elapsed;
        let eta = if total > 0 && speed > 0.0 {
            let r = (total.saturating_sub(bytes)) as f64 / speed;
            fmt_dur(Duration::from_secs(r as u64))
        } else {
            "--:--".into()
        };

        eprint!(
            "\r  {bar} {:5.1}%  {:>12} строк  {:>9}  {:>9}/с  eta {:>5}  {}   ",
            pct * 100.0,
            fmt_thousands(lines),
            fmt_size(bytes),
            fmt_size(speed as u64),
            eta,
            phase
        );
        io::stderr().flush().ok();
    }

    /// Финальное сообщение фазы: что сделано и за какое время.
    pub fn phase_done(&self, what: &str) {
        if self.enabled {
            self.end_line();
            let d = self.phase_start.lock().unwrap().elapsed();
            eprintln!("    готово: {what} за {}", fmt_dur(d));
        }
        *self.phase_start.lock().unwrap() = Instant::now();
    }

    /// Печать в не-терминал: раз в 10 секунд однострочный отчёт.
    pub fn log_line(&self, extra: &str) {
        if !self.enabled || self.tty {
            return;
        }
        let mut last = self.last_paint.lock().unwrap();
        if last.elapsed() < Duration::from_secs(10) {
            return;
        }
        *last = Instant::now();
        drop(last);
        let bytes = self.bytes.load(AtomicOrdering::Relaxed);
        let lines = self.lines.load(AtomicOrdering::Relaxed);
        let phase = self.phase.lock().unwrap().clone();
        eprintln!("[{phase}] строк: {} байт: {} {extra}", fmt_thousands(lines), fmt_size(bytes));
    }

    /// Тихий режим? (для внутренних проверок)
    pub fn quiet(&self) -> bool {
        !self.enabled
    }
}

// ============================================================================
//  encoding — декодирование cp1251 / koi8-r / ibm866 в UTF-8
// ============================================================================

mod encoding {
    // ==== ТАБЛИЦЫ КОДИРОВОК (сгенерировано scripts/gen_tables.py, не править руками) ====
    /// cp1251: байт -> Unicode (0xFFFD = байт не определён в кодировке; неопределённые: [152])
    pub const CP1251_TO_UNICODE: [u16; 256] = [0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0017, 0x0018, 0x0019, 0x001A, 0x001B, 0x001C, 0x001D, 0x001E, 0x001F, 0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F, 0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F, 0x0060, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078, 0x0079, 0x007A, 0x007B, 0x007C, 0x007D, 0x007E, 0x007F, 0x0402, 0x0403, 0x201A, 0x0453, 0x201E, 0x2026, 0x2020, 0x2021, 0x20AC, 0x2030, 0x0409, 0x2039, 0x040A, 0x040C, 0x040B, 0x040F, 0x0452, 0x2018, 0x2019, 0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0xFFFD, 0x2122, 0x0459, 0x203A, 0x045A, 0x045C, 0x045B, 0x045F, 0x00A0, 0x040E, 0x045E, 0x0408, 0x00A4, 0x0490, 0x00A6, 0x00A7, 0x0401, 0x00A9, 0x0404, 0x00AB, 0x00AC, 0x00AD, 0x00AE, 0x0407, 0x00B0, 0x00B1, 0x0406, 0x0456, 0x0491, 0x00B5, 0x00B6, 0x00B7, 0x0451, 0x2116, 0x0454, 0x00BB, 0x0458, 0x0405, 0x0455, 0x0457, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F, 0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F];

    /// koi8-r: байт -> Unicode (0xFFFD = байт не определён в кодировке; неопределённые: [])
    pub const KOI8R_TO_UNICODE: [u16; 256] = [0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0017, 0x0018, 0x0019, 0x001A, 0x001B, 0x001C, 0x001D, 0x001E, 0x001F, 0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F, 0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F, 0x0060, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078, 0x0079, 0x007A, 0x007B, 0x007C, 0x007D, 0x007E, 0x007F, 0x2500, 0x2502, 0x250C, 0x2510, 0x2514, 0x2518, 0x251C, 0x2524, 0x252C, 0x2534, 0x253C, 0x2580, 0x2584, 0x2588, 0x258C, 0x2590, 0x2591, 0x2592, 0x2593, 0x2320, 0x25A0, 0x2219, 0x221A, 0x2248, 0x2264, 0x2265, 0x00A0, 0x2321, 0x00B0, 0x00B2, 0x00B7, 0x00F7, 0x2550, 0x2551, 0x2552, 0x0451, 0x2553, 0x2554, 0x2555, 0x2556, 0x2557, 0x2558, 0x2559, 0x255A, 0x255B, 0x255C, 0x255D, 0x255E, 0x255F, 0x2560, 0x2561, 0x0401, 0x2562, 0x2563, 0x2564, 0x2565, 0x2566, 0x2567, 0x2568, 0x2569, 0x256A, 0x256B, 0x256C, 0x00A9, 0x044E, 0x0430, 0x0431, 0x0446, 0x0434, 0x0435, 0x0444, 0x0433, 0x0445, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F, 0x044F, 0x0440, 0x0441, 0x0442, 0x0443, 0x0436, 0x0432, 0x044C, 0x044B, 0x0437, 0x0448, 0x044D, 0x0449, 0x0447, 0x044A, 0x042E, 0x0410, 0x0411, 0x0426, 0x0414, 0x0415, 0x0424, 0x0413, 0x0425, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x042F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0416, 0x0412, 0x042C, 0x042B, 0x0417, 0x0428, 0x042D, 0x0429, 0x0427, 0x042A];

    /// cp866: байт -> Unicode (0xFFFD = байт не определён в кодировке; неопределённые: [])
    pub const IBM866_TO_UNICODE: [u16; 256] = [0x0000, 0x0001, 0x0002, 0x0003, 0x0004, 0x0005, 0x0006, 0x0007, 0x0008, 0x0009, 0x000A, 0x000B, 0x000C, 0x000D, 0x000E, 0x000F, 0x0010, 0x0011, 0x0012, 0x0013, 0x0014, 0x0015, 0x0016, 0x0017, 0x0018, 0x0019, 0x001A, 0x001B, 0x001C, 0x001D, 0x001E, 0x001F, 0x0020, 0x0021, 0x0022, 0x0023, 0x0024, 0x0025, 0x0026, 0x0027, 0x0028, 0x0029, 0x002A, 0x002B, 0x002C, 0x002D, 0x002E, 0x002F, 0x0030, 0x0031, 0x0032, 0x0033, 0x0034, 0x0035, 0x0036, 0x0037, 0x0038, 0x0039, 0x003A, 0x003B, 0x003C, 0x003D, 0x003E, 0x003F, 0x0040, 0x0041, 0x0042, 0x0043, 0x0044, 0x0045, 0x0046, 0x0047, 0x0048, 0x0049, 0x004A, 0x004B, 0x004C, 0x004D, 0x004E, 0x004F, 0x0050, 0x0051, 0x0052, 0x0053, 0x0054, 0x0055, 0x0056, 0x0057, 0x0058, 0x0059, 0x005A, 0x005B, 0x005C, 0x005D, 0x005E, 0x005F, 0x0060, 0x0061, 0x0062, 0x0063, 0x0064, 0x0065, 0x0066, 0x0067, 0x0068, 0x0069, 0x006A, 0x006B, 0x006C, 0x006D, 0x006E, 0x006F, 0x0070, 0x0071, 0x0072, 0x0073, 0x0074, 0x0075, 0x0076, 0x0077, 0x0078, 0x0079, 0x007A, 0x007B, 0x007C, 0x007D, 0x007E, 0x007F, 0x0410, 0x0411, 0x0412, 0x0413, 0x0414, 0x0415, 0x0416, 0x0417, 0x0418, 0x0419, 0x041A, 0x041B, 0x041C, 0x041D, 0x041E, 0x041F, 0x0420, 0x0421, 0x0422, 0x0423, 0x0424, 0x0425, 0x0426, 0x0427, 0x0428, 0x0429, 0x042A, 0x042B, 0x042C, 0x042D, 0x042E, 0x042F, 0x0430, 0x0431, 0x0432, 0x0433, 0x0434, 0x0435, 0x0436, 0x0437, 0x0438, 0x0439, 0x043A, 0x043B, 0x043C, 0x043D, 0x043E, 0x043F, 0x2591, 0x2592, 0x2593, 0x2502, 0x2524, 0x2561, 0x2562, 0x2556, 0x2555, 0x2563, 0x2551, 0x2557, 0x255D, 0x255C, 0x255B, 0x2510, 0x2514, 0x2534, 0x252C, 0x251C, 0x2500, 0x253C, 0x255E, 0x255F, 0x255A, 0x2554, 0x2569, 0x2566, 0x2560, 0x2550, 0x256C, 0x2567, 0x2568, 0x2564, 0x2565, 0x2559, 0x2558, 0x2552, 0x2553, 0x256B, 0x256A, 0x2518, 0x250C, 0x2588, 0x2584, 0x258C, 0x2590, 0x2580, 0x0440, 0x0441, 0x0442, 0x0443, 0x0444, 0x0445, 0x0446, 0x0447, 0x0448, 0x0449, 0x044A, 0x044B, 0x044C, 0x044D, 0x044E, 0x044F, 0x0401, 0x0451, 0x0404, 0x0454, 0x0407, 0x0457, 0x040E, 0x045E, 0x00B0, 0x2219, 0x00B7, 0x221A, 0x2116, 0x00A4, 0x25A0, 0x00A0];

    /// Поддерживаемые кодировки входных файлов.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Encoding {
        Utf8,
        Cp1251,
        Koi8r,
        Ibm866,
    }

    impl Encoding {
        pub fn parse(s: &str) -> Option<Encoding> {
            match s.to_ascii_lowercase().as_str() {
                "utf8" | "utf-8" | "utf" => Some(Encoding::Utf8),
                "cp1251" | "windows-1251" | "win1251" => Some(Encoding::Cp1251),
                "koi8r" | "koi8-r" | "koi8" => Some(Encoding::Koi8r),
                "ibm866" | "cp866" | "dos866" | "866" => Some(Encoding::Ibm866),
                _ => None,
            }
        }
    }
}

pub use encoding::Encoding;

impl Encoding {
    /// Таблица байт->кодовая точка Unicode (None для UTF-8).
    fn table(self) -> Option<&'static [u16; 256]> {
        match self {
            Encoding::Utf8 => None,
            Encoding::Cp1251 => Some(&encoding::CP1251_TO_UNICODE),
            Encoding::Koi8r => Some(&encoding::KOI8R_TO_UNICODE),
            Encoding::Ibm866 => Some(&encoding::IBM866_TO_UNICODE),
        }
    }

    /// Декодировать строку из входной кодировки в UTF-8.
    /// Для UTF-8 — прямое копирование (строка уже валидна по построению:
    /// мы режем по байту '\n', не разрезая многобайтовые последовательности).
    pub fn decode(&self, src: &[u8], dst: &mut Vec<u8>) {
        match self.table() {
            None => dst.extend_from_slice(src),
            Some(table) => {
                dst.reserve(src.len() + src.len() / 4);
                for &b in src {
                    let cp = table[b as usize] as u32;
                    // Кодовые точки этих кодировок не выходят за BMP,
                    // поэтому достаточно веток на 1/2/3 байта UTF-8.
                    if cp < 0x80 {
                        dst.push(cp as u8);
                    } else if cp < 0x800 {
                        dst.push(0xC0 | (cp >> 6) as u8);
                        dst.push(0x80 | (cp & 0x3F) as u8);
                    } else {
                        dst.push(0xE0 | (cp >> 12) as u8);
                        dst.push(0x80 | ((cp >> 6) & 0x3F) as u8);
                        dst.push(0x80 | (cp & 0x3F) as u8);
                    }
                }
            }
        }
    }
}

// ============================================================================
//  simd — быстрый поиск байта '\n' в буфере
// ============================================================================
//  Горячая точка конвейера: на каждый прочитанный байт нужно найти границы
//  строк. Используем векторные инструкции x86 (AVX2 = 32 байта/такт,
//  SSE2 = 16), выбранные один раз при старте. На остальных архитектурах —
//  скалярный цикл (компилятор всё равно автовекторизует его частично).

mod simd {
    /// Уровень: 0 — скалярно, 1 — SSE2, 2 — AVX2. Определяется один раз.
    fn level() -> u8 {
        static LV: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
        *LV.get_or_init(|| {
            #[cfg(target_arch = "x86_64")]
            {
                if std::arch::is_x86_feature_detected!("avx2") {
                    2
                } else if std::arch::is_x86_feature_detected!("sse2") {
                    1
                } else {
                    0
                }
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                0
            }
        })
    }

    /// Найти индекс первого вхождения байта needle в buf (None, если нет).
    /// Обобщение find_newline: используется и для '\n' (границы строк),
    /// и для '\x00' (разделитель ключ/номер в режиме --no-sort).
    #[inline]
    pub fn find_byte(buf: &[u8], needle: u8) -> Option<usize> {
        match level() {
            2 => unsafe { find_avx2(buf, needle) },
            1 => unsafe { find_sse2(buf, needle) },
            _ => buf.iter().position(|&b| b == needle),
        }
    }

    /// SSE2: обрабатываем по 16 байт, хвост — скалярно.
    ///
    /// # Safety
    /// Требуется CPU с SSE2 (проверено в level()).
    #[cfg(target_arch = "x86_64")]
    unsafe fn find_sse2(buf: &[u8], needle: u8) -> Option<usize> {
        use std::arch::x86_64::*;
        let n = buf.len();
        let nl = _mm_set1_epi8(needle as i8);
        let mut i = 0;
        while i + 16 <= n {
            let chunk = _mm_loadu_si128(buf.as_ptr().add(i) as *const __m128i);
            let mask = _mm_movemask_epi8(_mm_cmpeq_epi8(chunk, nl)) as u32;
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 16;
        }
        buf[i..].iter().position(|&b| b == needle).map(|p| p + i)
    }

    /// AVX2: обрабатываем по 32 байта, хвост — скалярно.
    ///
    /// # Safety
    /// Требуется CPU с AVX2 (проверено в level()).
    #[cfg(target_arch = "x86_64")]
    unsafe fn find_avx2(buf: &[u8], needle: u8) -> Option<usize> {
        use std::arch::x86_64::*;
        let n = buf.len();
        let nl = _mm256_set1_epi8(needle as i8);
        let mut i = 0;
        while i + 32 <= n {
            let chunk = _mm256_loadu_si256(buf.as_ptr().add(i) as *const __m256i);
            let mask = _mm256_movemask_epi8(_mm256_cmpeq_epi8(chunk, nl));
            if mask != 0 {
                return Some(i + mask.trailing_zeros() as usize);
            }
            i += 32;
        }
        buf[i..].iter().position(|&b| b == needle).map(|p| p + i)
    }

    // Запасные определения для не-x86_64: level() уже вернул 0, сюда
    // управление не попадёт, но код должен компилироваться.
    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn find_sse2(_buf: &[u8]) -> Option<usize> {
        unreachable!()
    }
    #[cfg(not(target_arch = "x86_64"))]
    unsafe fn find_avx2(_buf: &[u8]) -> Option<usize> {
        unreachable!()
    }
}

// ============================================================================
//  io — построчный ввод-вывод с ограничением памяти
// ============================================================================

/// Построчный читатель одного файла: читает большими блоками (8 МиБ),
/// границы строк ищет SIMD-поиском '\n', выдаёт строки без завершающего
/// '\n'. Строка помещается в буфер вызывающего (переиспользуемый
/// Vec<u8>), поэтому нет аллокаций на строку и нет проблем времени
/// жизни; строки могут быть сколь угодно длинными (дорасширяют буфер).
pub struct LineReader<R: Read> {
    inner: R,
    /// буфер чтения
    buf: Vec<u8>,
    /// позиция разбора в buf
    pos: usize,
    /// сколько валидных байт в buf
    filled: usize,
    /// EOF уже достигнут
    eof: bool,
    /// прочитано байт из файла (для прогресса)
    pub bytes_read: u64,
}

/// Буфер чтения входных файлов: большой (SIMD успевает отработать,
/// чтение последовательное).
pub const INPUT_BUF: usize = 8 * 1024 * 1024;

/// Буфер читателя серии при слиянии: серии открываются пачкой до fanin
/// штук, поэтому буфер небольшой — fanin * 512 КиБ <= 32 МиБ при fanin=64.
pub const MERGE_BUF: usize = 512 * 1024;

impl<R: Read> LineReader<R> {
    pub fn new(inner: R) -> Self {
        LineReader::with_capacity(inner, INPUT_BUF)
    }

    /// Читатель с заданным размером буфера (сериям слияния — меньше).
    pub fn with_capacity(inner: R, cap: usize) -> Self {
        LineReader {
            inner,
            buf: vec![0; cap],
            pos: 0,
            filled: 0,
            eof: false,
            bytes_read: 0,
        }
    }

    /// Заполнить буфер новым блоком. Возвращает false при EOF.
    /// Вызывается, когда buf полностью разобран (pos == filled).
    /// Read может вернуть меньше запрошенного без EOF — дочитываем
    /// до полного буфера, не затирая уже прочитанное.
    fn fill(&mut self) -> Result<bool, io::Error> {
        debug_assert_eq!(self.pos, self.filled);
        self.filled = 0;
        self.pos = 0;
        loop {
            let n = self.inner.read(&mut self.buf[self.filled..])?;
            self.bytes_read += n as u64;
            if n == 0 {
                self.eof = true;
                if self.filled == 0 {
                    return Ok(false);
                }
                return Ok(true); // есть данные, EOF при следующем fill
            }
            self.filled += n;
            if self.filled == self.buf.len() {
                return Ok(true);
            }
        }
    }

    /// Следующая строка без '\n': копируется в out (out очищается).
    /// Возвращает false в конце файла. Строки с CR: если строка
    /// заканчивается на '\r' (CRLF-файл), '\r' тоже отбрасывается.
    pub fn next_line(&mut self, out: &mut Vec<u8>) -> Result<bool, io::Error> {
        out.clear();
        loop {
            if self.pos < self.filled {
                if let Some(i) = simd::find_byte(&self.buf[self.pos..self.filled], b'\n') {
                    let end = self.pos + i;
                    // Копируем строку из буфера в out (одна аллокация
                    // разрастания на длинную строку, обычно ноль).
                    out.extend_from_slice(&self.buf[self.pos..end]);
                    self.pos = end + 1;
                    // CRLF -> LF: убираем хвостовой '\r'.
                    if out.last() == Some(&b'\r') {
                        out.pop();
                    }
                    return Ok(true);
                }
                // В буфере нет '\n': копируем остаток в out, читаем дальше.
                out.extend_from_slice(&self.buf[self.pos..self.filled]);
                self.pos = self.filled;
            }
            if self.eof {
                if !out.is_empty() {
                    // Последняя строка без завершающего перевода строки.
                    if out.last() == Some(&b'\r') {
                        out.pop();
                    }
                    return Ok(true);
                }
                return Ok(false);
            }
            self.fill()?;
        }
    }
}

/// Многофайловый читатель: выдаёт строки всех файлов по очереди.
/// Нужен для режима --merge (каталог склеивается в один поток строк).
pub struct MultiReader {
    /// читатели всех файлов (текущий — первый живой)
    readers: Vec<LineReader<File>>,
    cur_idx: usize,
    /// сумма bytes_read по завершённым файлам
    done_bytes: u64,
}

impl MultiReader {
    /// Создать читатель; files должен быть непустым (проверено выше).
    pub fn new(files: Vec<File>) -> Self {
        let readers = files.into_iter().map(LineReader::new).collect();
        MultiReader {
            readers,
            cur_idx: 0,
            done_bytes: 0,
        }
    }

    /// Следующая строка (аналог LineReader::next_line).
    pub fn next_line(&mut self, out: &mut Vec<u8>) -> Result<bool, io::Error> {
        loop {
            if self.cur_idx >= self.readers.len() {
                return Ok(false);
            }
            if self.readers[self.cur_idx].next_line(out)? {
                return Ok(true);
            }
            // Файл исчерпан: переход к следующему.
            self.done_bytes += self.readers[self.cur_idx].bytes_read;
            self.cur_idx += 1;
        }
    }

    /// Суммарно прочитано байт (для прогресса).
    pub fn bytes_read(&self) -> u64 {
        let cur = self
            .readers
            .get(self.cur_idx)
            .map(|r| r.bytes_read)
            .unwrap_or(0);
        self.done_bytes + cur
    }
}

/// Писатель выходного файла с разбивкой на части.
///
/// Части создаются по двум независимым критериям:
///   * `part_lines` — максимум строк в части (None = без ограничения);
///   * `part_bytes` — максимум байт в части (считая '\n').
/// Строка никогда не разрезается: если не влезает по байтам, целиком
/// уходит в следующую часть (поэтому часть может превысить лимит на
/// длину одной строки — это задокументированное поведение).
///
/// Всё пишется во временный файл `.part` с последующим rename:
/// если процесс умрёт, на диске не окажется «наполовину готового»
/// результата под окончательным именем.
pub struct PartWriter {
    dir: PathBuf,
    /// шаблон имени: namepartN.ext -> подставляем номер части
    stem: String,
    ext: String,
    part_lines: Option<u64>,
    part_bytes: Option<u64>,
    /// текущий номер части (1-based)
    part_no: u64,
    /// строк/байт записано в текущую часть
    cur_lines: u64,
    cur_bytes: u64,
    /// уже созданные временные файлы (для rename в finish)
    tmp_paths: Vec<(PathBuf, PathBuf)>, // (tmp, final)
    writer: Option<BufWriter<File>>,
    /// статистика для отчёта
    pub total_lines: u64,
    pub total_bytes: u64,
    /// единый прогресс
    prog: std::sync::Arc<Progress>,
}

impl PartWriter {
    /// Создать писатель.
    ///  * path     — итоговое имя; при разбивке из него делается
    ///               name.partNNN.ext, без разбивки — имя как есть;
    ///  * part_lines / part_bytes — лимиты части (None = нет лимита).
    pub fn new(
        path: &Path,
        part_lines: Option<u64>,
        part_bytes: Option<u64>,
        prog: std::sync::Arc<Progress>,
    ) -> Res<Self> {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "out".into());
        let ext = path
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let dir = dir_for(&path.parent().map(|p| p.to_path_buf()).unwrap_or_default());
        let split = part_lines.is_some() || part_bytes.is_some();
        let (f, tmp, fin) = Self::open_tmp(&dir, &stem, &ext, 1, split)?;
        let tmp_paths = vec![(tmp, fin)];
        Ok(PartWriter {
            dir,
            stem,
            ext,
            part_lines,
            part_bytes,
            part_no: 1,
            cur_lines: 0,
            cur_bytes: 0,
            tmp_paths,
            writer: Some(Self::wrap(f)?),
            total_lines: 0,
            total_bytes: 0,
            prog,
        })
    }

    fn wrap(f: File) -> Res<BufWriter<File>> {
        Ok(BufWriter::with_capacity(8 * 1024 * 1024, f))
    }

    /// Открыть временный файл для части part_no.
    /// Возвращает (файл, tmp-путь, окончательный путь).
    fn open_tmp(dir: &Path, stem: &str, ext: &str, part_no: u64, split: bool) -> Res<(File, PathBuf, PathBuf)> {
        let fin = if split {
            part_final_name(dir, stem, ext, Some(part_no))
        } else {
            part_final_name(dir, stem, ext, None)
        };
        let mut tmp = fin.clone();
        // tmp-имя: stem.partNNN.ext.part — даже при коллизии с
        // существующими файлами не затронет готовые данные.
        tmp.set_extension(format!("{}{}", ext.trim_start_matches('.'), ".part"));
        let f = File::create(&tmp).map_err(|e| {
            AppError::Io(format!("не удалось создать {}: {e}", tmp.display()))
        })?;
        Ok((f, tmp, fin))
    }

    /// Записать строку (без '\n') как отдельную строку файла.
    pub fn write_line(&mut self, line: &[u8]) -> Res<()> {
        // Нужно ли открыть новую часть? Критерий: текущая часть уже
        // непуста и превышает хотя бы один лимит после записи строки.
        let nl = line.len() as u64 + 1; // + '\n'
        let over_lines = self
            .part_lines
            .is_some_and(|m| self.cur_lines >= m);
        let over_bytes = self
            .part_bytes
            .is_some_and(|m| self.cur_bytes > 0 && self.cur_bytes + nl > m);
        if over_lines || over_bytes {
            self.rollover()?;
        }
        let w = self.writer.as_mut().ok_or_else(|| {
            AppError::Io("писатель уже закрыт (внутренняя ошибка)".into())
        })?;
        w.write_all(line)?;
        w.write_all(b"\n")?;
        self.cur_lines += 1;
        self.cur_bytes += nl;
        self.total_lines += 1;
        self.total_bytes += nl;
        Ok(())
    }

    /// Закрыть текущую часть и открыть следующую.
    fn rollover(&mut self) -> Res<()> {
        if let Some(w) = self.writer.take() {
            let f = w.into_inner().map_err(|e| {
                AppError::Io(format!("сбой записи: {e}"))
            })?;
            f.sync_all().ok();
        }
        self.part_no += 1;
        self.cur_lines = 0;
        self.cur_bytes = 0;
        let (f, tmp, fin) = Self::open_tmp(&self.dir, &self.stem, &self.ext, self.part_no, true)?;
        self.tmp_paths.push((tmp, fin));
        self.writer = Some(Self::wrap(f)?);
        Ok(())
    }

    /// Завершить запись: закрыть часть(и), переименовать .part -> финал.
    /// Возвращает число созданных файлов-частей.
    pub fn finish(&mut self) -> Res<u64> {
        if let Some(w) = self.writer.take() {
            let f = w
                .into_inner()
                .map_err(|e| AppError::Io(format!("сбой записи: {e}")))?;
            f.sync_all().ok();
        }
        let n = self.tmp_paths.len() as u64;
        for (tmp, fin) in std::mem::take(&mut self.tmp_paths) {
            fs::rename(&tmp, &fin).map_err(|e| {
                AppError::Io(format!(
                    "не удалось переименовать {} -> {}: {e}",
                    tmp.display(),
                    fin.display()
                ))
            })?;
            self.prog.vmsg(&format!("часть {} готова", fin.display()));
        }
        Ok(n)
    }

    /// Текущее число частей (для прогресса).
    pub fn part_no(&self) -> u64 {
        self.part_no
    }

    /// Каталог записи (для отчёта).
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// Собрать имя части: stem.partNNN.ext (или stem.ext для единого файла).
fn part_final_name(dir: &Path, stem: &str, ext: &str, part: Option<u64>) -> PathBuf {
    match part {
        None => dir.join(format!("{stem}{ext}")),
        Some(p) => dir.join(format!("{stem}.part{p:03}{ext}")),
    }
}

/// Нормализовать каталог (пустой родитель = текущий каталог).
fn dir_for(d: &Path) -> PathBuf {
    if d.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        d.to_path_buf()
    }
}

// ============================================================================
//  arena — арена строк и лёгкие ссылки на строки
// ============================================================================

/// Жёсткий потолок объёма строк одной серии: смещения в LineRef — u32,
/// поэтому серия не может быть больше 4 ГиБ. Запас в 64 КиБ снимает
/// краевые эффекты при закрытии серии (входящая строка + ссылка).
pub const ARENA_MAX: usize = (u32::MAX as usize) - (64 * 1024);

/// Арена (bump-аллокатор) байтов: строки одной серии складываются в
/// одну большую Vec<u8>, а сортируются только лёгкие 8-байтовые ссылки
/// LineRef { off: u32, len: u32 }. Это убирает сотни миллионов
/// аллокаций String и делает сортировку cache-friendly.
///
/// Смещения в LineRef — u32, поэтому суммарный объём строк одной серии
/// не может превышать 4 ГиБ: при выходе за предел серия закрывается
/// досрочно (ExternalSorter проверяет can_push перед каждым push) —
/// корректность важнее идеального размера серии.
pub struct Arena {
    data: Vec<u8>,
    /// лимит байтов арены (бюджет серии минус служебные структуры)
    cap: usize,
}

impl Arena {
    pub fn new(cap: usize) -> Self {
        Arena {
            data: Vec::with_capacity(1024 * 1024),
            // Пользовательский бюджет не может раздвинуть u32-потолок
            // смещений LineRef: реальный предел — минимум из двух.
            cap: cap.min(ARENA_MAX),
        }
    }

    /// Добавить строку, вернуть ссылку на неё.
    /// ТРЕБУЕТ: строка короче 4 ГиБ и сумма уже записанного + строки
    /// не превышает 4 ГиБ (гарантируется проверкой can_push у вызывающего).
    pub fn push(&mut self, line: &[u8]) -> LineRef {
        let off = self.data.len() as u32;
        debug_assert!(off as usize + line.len() <= u32::MAX as usize);
        self.data.extend_from_slice(line);
        LineRef {
            off,
            len: line.len() as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Занято байт (для контроля бюджета).
    pub fn used(&self) -> usize {
        self.data.len()
    }

    /// Лимит байтов арены.
    pub fn cap(&self) -> usize {
        self.cap
    }

    /// Получить строку по ссылке.
    #[inline]
    pub fn get(&self, r: LineRef) -> &[u8] {
        &self.data[r.off as usize..(r.off + r.len) as usize]
    }

    /// Забрать внутренний буфер арены (потребляется при передаче серии
    /// в сортировку долей — арена становится Arc<Vec<u8>>).
    pub fn into_data(self) -> Vec<u8> {
        self.data
    }
}

/// Ссылка на строку в арене: 8 байт (u32-смещение + u32-длина).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct LineRef {
    pub off: u32,
    pub len: u32,
}

// ============================================================================
//  cmp — компараторы строк
// ============================================================================

/// Режим сравнения строк.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CmpMode {
    /// Байтовое сравнение UTF-8: строгий порядок Unicode, детерминирован.
    /// Кириллица в UTF-8 упорядочивается по кодовым точкам, что для
    /// русского алфавита совпадает с алфавитным порядком (а-я), ё
    /// находится после е — как в словарях Unicode (default collation).
    Bytes,
    /// Естественная сортировка: последовательности цифр сравниваются
    /// как числа ("файл2" < "файл10"), остальное — байтово.
    Natural,
    /// Регистронезависимое: ASCII-буквы сравниваются без учёта регистра,
    /// при равенстве — байтовое сравнение (детерминированность).
    CaseFold,
}

/// Сравнение строк по режиму (восходящее).
#[inline]
pub fn cmp_mode(a: &[u8], b: &[u8], mode: CmpMode) -> Ordering {
    match mode {
        CmpMode::Bytes | CmpMode::CaseFold => {
            let n = a.len().min(b.len());
            if mode == CmpMode::CaseFold {
                // Регистронезависимое сравнение общих префиксов; ASCII-буквы
                // приводятся к нижнему регистру, остальные байты — как есть
                // (кириллица UTF-8 регистронезависимо не сравнивается —
                // это задокументированное ограничение, как у GNU sort -f).
                let mut i = 0;
                while i < n {
                    let (la, lb) = (a[i].to_ascii_lowercase(), b[i].to_ascii_lowercase());
                    if la != lb {
                        return la.cmp(&lb);
                    }
                    i += 1;
                }
                // Префиксы совпали без учёта регистра: сначала по длине,
                // затем байтовое сравнение (детерминированный порядок
                // "Apple" < "apple" при равенстве без регистра).
                match a.len().cmp(&b.len()) {
                    Equal => a.cmp(b),
                    o => o,
                }
            } else {
                // Байтовое: memcmp общих префиксов, затем по длине.
                match a[..n].cmp(&b[..n]) {
                    Equal => a.len().cmp(&b.len()),
                    o => o,
                }
            }
        }
        CmpMode::Natural => natural_cmp(a, b),
    }
}

/// Естественное сравнение: цифровые группы — как числа, остальное — байтово.
/// Реализовано без аллокаций и без разбора UTF-8 (цифры ASCII однобайтовые,
/// кириллица UTF-8 не содержит байтов 0x30-0x39 в продолжениях — 10xxxxxx).
pub fn natural_cmp(a: &[u8], b: &[u8]) -> Ordering {
    let (mut i, mut j) = (0usize, 0usize);
    loop {
        let a_digit = i < a.len() && a[i].is_ascii_digit();
        let b_digit = j < b.len() && b[j].is_ascii_digit();
        if a_digit && b_digit {
            // Пропускаем ведущие нули, сравниваем значащие разряды.
            let mut ia = i;
            let mut ja = j;
            while ia < a.len() && a[ia] == b'0' {
                ia += 1;
            }
            while ja < b.len() && b[ja] == b'0' {
                ja += 1;
            }
            let mut ea = ia;
            let mut eb = ja;
            while ea < a.len() && a[ea].is_ascii_digit() {
                ea += 1;
            }
            while eb < b.len() && b[eb].is_ascii_digit() {
                eb += 1;
            }
            let (da, db) = (&a[ia..ea], &b[ja..eb]);
            match da.len().cmp(&db.len()) {
                Equal => match da.cmp(db) {
                    Equal => {
                        // Числа равны: сначала длина исходной группы с нулями,
                        // чтобы "07" отличалось от "7" детерминированно.
                        i = ea;
                        j = eb;
                    }
                    o => return o,
                },
                o => return o,
            }
        } else if i >= a.len() && j >= b.len() {
            return Equal;
        } else if i >= a.len() {
            return Less;
        } else if j >= b.len() {
            return Greater;
        } else {
            match a[i].cmp(&b[j]) {
                Equal => {
                    i += 1;
                    j += 1;
                }
                o => return o,
            }
        }
    }
}

/// Направление сортировки.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Direction {
    Asc,
    Desc,
}

// ============================================================================
//  chunksort — параллельная сортировка долей серии + heap-слияние долей
// ============================================================================
//  Одна серия (run) внешней сортировки — до run_budget байт строк в арене.
//  Сортировать 30 млн ссылок одним sort_by — долго (один поток). Вместо
//  этого ссылки делятся на jobs долей, каждая сортируется в своём потоке
//  (std::thread::scope — заимствования без 'static), затем доли сливаются
//  лениво кучей при записи серии на диск. Серия остаётся упорядоченной,
//  память не дублируется (доли разделяют арену через Arc), а сортировка
//  использует все ядра.

/// Отсортированная доля серии: ссылки на строки в общей арене.
pub struct Chunk {
    refs: Vec<LineRef>,
}

/// Безопасное чтение строки из арены по ссылке.
#[inline]
fn arena_get(data: &[u8], r: LineRef) -> &[u8] {
    &data[r.off as usize..(r.off + r.len) as usize]
}

/// Сортировка одной доли ссылок (вызывается в потоках).
fn sort_part(part: &mut [LineRef], arena: &[u8], mode: CmpMode, desc: bool) {
    if desc {
        part.sort_by(|a, b| {
            cmp_mode(arena_get(arena, *a), arena_get(arena, *b), mode).reverse()
        });
    } else {
        part.sort_by(|a, b| cmp_mode(arena_get(arena, *a), arena_get(arena, *b), mode));
    }
}

/// Элемент кучи слияния: строка + индекс источника. BinaryHeap — max-heap
/// по Ord, а на вершине нам нужен «следующий по порядку», поэтому Ord
/// строится инверсией целевого порядка (см. cmp_for_heap).
struct MergeItem<'a> {
    line: &'a [u8],
    from: usize,
}

impl<'a> PartialEq for MergeItem<'a> {
    fn eq(&self, o: &Self) -> bool {
        self.line == o.line && self.from == o.from
    }
}
impl<'a> Eq for MergeItem<'a> {}
impl<'a> PartialOrd for MergeItem<'a> {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl<'a> Ord for MergeItem<'a> {
    fn cmp(&self, o: &Self) -> Ordering {
        cmp_for_heap(self.line, o.line, self.from, o.from)
    }
}

thread_local! {
    /// Порядок слияния текущего потока: (режим сравнения, направление).
    /// Устанавливается перед использованием кучи в этом потоке.
    static MERGE_ORDER: std::cell::RefCell<Option<(CmpMode, bool)>> =
        const { std::cell::RefCell::new(None) };
}

/// Сравнение для кучи: по строке, при равенстве — по индексу источника.
/// Для asc порядок инвертируется (куча — max-heap, нам нужна min-вершина).
#[inline]
fn cmp_for_heap(a: &[u8], b: &[u8], fa: usize, fb: usize) -> Ordering {
    MERGE_ORDER.with(|cell| {
        let order = cell.borrow();
        let Some(&(mode, desc)) = order.as_ref() else {
            return a.cmp(b); // порядок не задан: byte-wise fallback
        };
        let o = cmp_mode(a, b, mode);
        let o = if desc { o } else { o.reverse() };
        match o {
            // Инверсия и для индекса: меньший индекс — выше при asc.
            Equal => fb.cmp(&fa),
            o => o,
        }
    })
}

/// Ленивое heap-слияние отсортированных долей серии: next() отдаёт
/// следующую строку итогового порядка. Доли разделяют арену (&[u8]).
pub struct ChunkMerger<'a> {
    chunks: Vec<Chunk>,
    pos: Vec<usize>,
    arena: &'a [u8],
    heap: BinaryHeap<MergeItem<'a>>,
}

impl<'a> ChunkMerger<'a> {
    /// Построить слияние по долей; доли уже отсортированы по mode/desc.
    pub fn new(chunks: Vec<Chunk>, arena: &'a [u8], mode: CmpMode, desc: bool) -> Self {
        MERGE_ORDER.with(|c| *c.borrow_mut() = Some((mode, desc)));
        let pos = vec![0; chunks.len()];
        let mut m = ChunkMerger {
            chunks,
            pos,
            arena,
            heap: BinaryHeap::with_capacity(16),
        };
        for i in 0..m.chunks.len() {
            if !m.chunks[i].refs.is_empty() {
                let line = arena_get(m.arena, m.chunks[i].refs[0]);
                m.heap.push(MergeItem { line, from: i });
            }
        }
        m
    }

    /// Следующая строка выходного порядка (None — серия исчерпана).
    pub fn next(&mut self) -> Option<&'a [u8]> {
        let top = self.heap.pop()?;
        let from = top.from;
        let p = self.pos[from];
        self.pos[from] = p + 1;
        if self.pos[from] < self.chunks[from].refs.len() {
            let line = arena_get(self.arena, self.chunks[from].refs[self.pos[from]]);
            self.heap.push(MergeItem { line, from });
        }
        Some(top.line)
    }
}

/// Параллельно отсортировать ссылки и вернуть отсортированные доли.
/// Доля = jobs; арена разделяется через Arc (доли не дублируют память).
pub fn sort_refs_parallel(
    arena: std::sync::Arc<Vec<u8>>,
    mut refs: Vec<LineRef>,
    mode: CmpMode,
    desc: bool,
    jobs: usize,
) -> Vec<Chunk> {
    let n = refs.len();
    if n == 0 {
        return Vec::new();
    }
    let jobs = jobs.max(1).min(n);
    if jobs == 1 {
        sort_part(&mut refs, &arena, mode, desc);
        return vec![Chunk { refs }];
    }
    // Ровные доли: первые n % jobs долей на одну ссылку длиннее.
    let base = n / jobs;
    let rem = n % jobs;
    let mut parts: Vec<Vec<LineRef>> = Vec::with_capacity(jobs);
    {
        let mut rest: &[LineRef] = &refs;
        for k in 0..jobs {
            let take = base + if k < rem { 1 } else { 0 };
            let (part, tail) = rest.split_at(take);
            parts.push(part.to_vec());
            rest = tail;
        }
    }
    drop(refs);
    let mut chunks: Vec<Chunk> = Vec::with_capacity(jobs);
    std::thread::scope(|s| {
        let mut handles = Vec::with_capacity(jobs);
        for mut part in parts {
            let arena = std::sync::Arc::clone(&arena);
            handles.push(s.spawn(move || {
                sort_part(&mut part, &arena, mode, desc);
                part
            }));
        }
        for h in handles {
            match h.join() {
                Ok(p) => chunks.push(Chunk { refs: p }),
                Err(_) => {
                    eprintln!("ошибка: поток сортировки упал");
                    std::process::exit(2);
                }
            }
        }
    });
    chunks
}

// ============================================================================
//  extsort — внешняя сортировка: серии, k-way merge, многопроходность
// ============================================================================
//  Классический external merge sort, адаптированный под нашу задачу:
//
//   Фаза 1 — распределение: читаем входной поток строк (уже декодирован
//   в UTF-8, обрезан/по-trim по настройкам), складываем в арену серии.
//   Бюджет серии исчерпан — параллельно сортируем доли и пишем серию
//   (run) во временный файл, освобождаем память. Повторяем до конца входа.
//
//   Фаза 2 — укрупнение: пока серий больше fanin (лимит открытых файлов,
//   по умолчанию 64), сливаем их группами по fanin в новые серии за один
//   проход; группы сливаются параллельно (по jobs потоков). Число
//   проходов ~ ceil(log_fanin(R)).
//
//   Фаза 3 — финальное слияние: k-сторонее слияние кучей всех серий
//   прямо в выходной писатель (с дедупликацией соседних повторов).
//
//   Дедупликация выполняется при записи: и внутри серии (после
//   сортировки дубликаты идут подряд), и на финальном слиянии (между
//   сериями). Ноль дополнительной памяти — достаточно сравнения с
//   предыдущей записанной строкой.

/// Временный файл серии: путь + размер в байтах.
struct RunFile {
    path: PathBuf,
    bytes: u64,
}

/// Итоговая статистика одной задачи сортировки (для отчёта).
#[derive(Default, Debug)]
pub struct Stats {
    /// строк прочитано из входа
    pub in_lines: u64,
    /// байт прочитано из входа
    pub in_bytes: u64,
    /// строк записано (после дедупа/фильтров)
    pub out_lines: u64,
    /// байт записано (с '\n')
    pub out_bytes: u64,
    /// удалено дубликатов (в сериях + на слияниях)
    pub dupes_removed: u64,
    /// создано серий
    pub runs: u64,
    /// проходов укрупнения
    pub merge_passes: u64,
}

/// Параметры внешней сортировки.
#[derive(Clone, Copy)]
pub struct ExtSortOpts {
    /// потоков сортировки (параллельные доли серии)
    pub jobs: usize,
    /// максимум серий, сливаемых за один проход (= лимит открытых файлов)
    pub fanin: usize,
}

/// Одна задача внешней сортировки: превращает поток строк в поток
/// отсортированных (и опционально дедуплицированных) строк.
pub struct ExternalSorter {
    tmp_dir: PathBuf,
    /// бюджет памяти на серию (арена + ссылки), байт
    run_budget: usize,
    mode: CmpMode,
    desc: bool,
    dedup: bool,
    prog: std::sync::Arc<Progress>,
    opts: ExtSortOpts,
    /// счётчик для уникальных имён временных файлов
    run_seq: std::sync::Arc<AtomicU64>,
}

impl ExternalSorter {
    pub fn new(
        tmp_dir: PathBuf,
        run_budget: usize,
        mode: CmpMode,
        desc: bool,
        dedup: bool,
        prog: std::sync::Arc<Progress>,
        opts: ExtSortOpts,
    ) -> Self {
        ExternalSorter {
            tmp_dir,
            run_budget,
            mode,
            desc,
            dedup,
            prog,
            opts,
            run_seq: std::sync::Arc::new(AtomicU64::new(0)),
        }
    }

    /// Уникальное имя временного файла серии (потокобезопасный счётчик).
    fn next_run_path(&self) -> PathBuf {
        let n = self.run_seq.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        self.tmp_dir.join(format!("linesort_run_{n:06}.tmp"))
    }

    /// Записать отсортированную серию в файл. Возвращает (RunFile,
    /// строк записано, дубликатов удалено внутри серии).
    fn write_sorted_run(
        &self,
        arena_data: Vec<u8>,
        refs: Vec<LineRef>,
        path: &Path,
    ) -> Res<(RunFile, u64, u64)> {
        let t0 = Instant::now();
        let arena = std::sync::Arc::new(arena_data);
        let chunks = sort_refs_parallel(
            std::sync::Arc::clone(&arena),
            refs,
            self.mode,
            self.desc,
            self.opts.jobs,
        );
        let f = File::create(path).map_err(|e| {
            AppError::Io(format!("не удалось создать {}: {e}", path.display()))
        })?;
        let mut f = BufWriter::with_capacity(4 * 1024 * 1024, f);
        let mut bytes: u64 = 0;
        let mut written: u64 = 0;
        let mut dupes: u64 = 0;
        {
            let mut merger = ChunkMerger::new(chunks, &arena, self.mode, self.desc);
            let mut prev: Option<Vec<u8>> = None;
            while let Some(line) = merger.next() {
                if self.dedup && prev.as_deref() == Some(line) {
                    dupes += 1;
                    continue;
                }
                f.write_all(line)?;
                f.write_all(b"\n")?;
                bytes += line.len() as u64 + 1;
                written += 1;
                match prev.as_mut() {
                    Some(p) => {
                        p.clear();
                        p.extend_from_slice(line);
                    }
                    None => prev = Some(line.to_vec()),
                }
            }
        }
        f.flush()?;
        drop(f);
        self.prog.vmsg(&format!(
            "серия {}: {} строк, {} ({:?})",
            path.display(),
            fmt_thousands(written),
            fmt_size(bytes),
            t0.elapsed()
        ));
        Ok((RunFile { path: path.to_path_buf(), bytes }, written, dupes))
    }

    /// Основной метод: поток входных строк (next) -> выходной писатель.
    /// total_in_bytes — ожидаемый объём чтения (для прогресса).
    pub fn run<F>(&mut self, mut next: F, out: &mut PartWriter, total_in_bytes: u64) -> Res<Stats>
    where
        F: FnMut(&mut Vec<u8>) -> Res<bool>,
    {
        // ---------- Фаза 1: распределение в серии ----------
        self.prog.phase("чтение и сортировка серий", total_in_bytes, 0);
        let mut stats = Stats::default();
        let mut runs: Vec<RunFile> = Vec::new();
        let mut arena = Arena::new(self.run_budget);
        let mut refs: Vec<LineRef> = Vec::new();
        let mut pending: Option<Vec<u8>> = None;
        loop {
            // Берём строку: сначала отложенную (не влезла в прошлую серию),
            // затем из входного потока.
            let line: Option<Vec<u8>> = match pending.take() {
                Some(p) => Some(p),
                None => {
                    let mut buf = Vec::new();
                    if next(&mut buf)? { Some(buf) } else { None }
                }
            };
            let Some(line) = line else { break };
            // Строка длиннее жёсткого потолка серии (4 ГиБ) не может
            // существовать в LineRef: длина там тоже u32. Такой вход —
            // не текстовые данные, честная ошибка вместо порчи данных.
            if line.len() > ARENA_MAX {
                return Err(AppError::Io(format!(
                    "строка № {} длиной {} превышает предел серии 4 ГиБ — вход не похож на текстовые строки",
                    stats.in_lines + 1,
                    fmt_size(line.len() as u64)
                )));
            }
            // Контроль бюджета серии: байты арены + 8 байт на ссылку.
            // Первая строка серии принимается всегда — серия обязана
            // прогрессировать (иначе сверхдлинная строка зависает).
            if !refs.is_empty() && arena.used() + line.len() + 8 > arena.cap() {
                let path = self.next_run_path();
                let old = std::mem::replace(&mut arena, Arena::new(self.run_budget));
                let (run, _w, d) =
                    self.write_sorted_run(old.into_data(), std::mem::take(&mut refs), &path)?;
                stats.dupes_removed += d;
                runs.push(run);
                pending = Some(line);
                continue;
            }
            refs.push(arena.push(&line));
            stats.in_lines += 1;
            self.prog.lines.fetch_add(1, AtomicOrdering::Relaxed);
            self.prog.tick();
        }
        // Хвостовая серия.
        if !refs.is_empty() {
            let path = self.next_run_path();
            let old = std::mem::replace(&mut arena, Arena::new(self.run_budget));
            let (run, _w, d) =
                self.write_sorted_run(old.into_data(), std::mem::take(&mut refs), &path)?;
            stats.dupes_removed += d;
            runs.push(run);
        }
        drop(arena);
        stats.runs = runs.len() as u64;
        stats.in_bytes = self.prog.bytes.load(AtomicOrdering::Relaxed);
        self.prog.phase_done(&format!(
            "прочитано {} строк, {} серий",
            fmt_thousands(stats.in_lines),
            runs.len()
        ));
        if runs.is_empty() {
            return Ok(stats); // пустой вход: выход создан пустым
        }

        // ---------- Фаза 2: укрупнение серий до fanin ----------
        let mut pass: u64 = 0;
        while runs.len() > self.opts.fanin {
            pass += 1;
            let total: u64 = runs.iter().map(|r| r.bytes).sum();
            self.prog.phase(
                &format!("проход слияния {pass}: укрупнение серий"),
                total,
                0,
            );
            // Разбиваем серии на группы по fanin.
            let mut groups: Vec<Vec<RunFile>> = Vec::new();
            let mut it = runs.into_iter();
            loop {
                let grp: Vec<RunFile> = it.by_ref().take(self.opts.fanin).collect();
                if grp.is_empty() {
                    break;
                }
                groups.push(grp);
            }
            // Параллельное слияние групп: до jobs групп одновременно.
            let merged = self.merge_groups_parallel(groups)?;
            runs = merged;
            stats.merge_passes = pass;
            self.prog.phase_done(&format!("осталось серий: {}", runs.len()));
        }

        // ---------- Фаза 3: финальное слияние в выход ----------
        let total: u64 = runs.iter().map(|r| r.bytes).sum();
        self.prog.phase("слияние и запись результата", total, 0);
        let dupes0 = stats.dupes_removed;
        let (out_lines, out_bytes, out_dupes) =
            merge_runs_to_writer(&runs, out, self.mode, self.desc, self.dedup, &self.prog)?;
        stats.out_lines = out_lines;
        stats.out_bytes = out_bytes;
        stats.dupes_removed = dupes0 + out_dupes;
        self.prog.phase_done(&format!(
            "записано {} строк ({}), удалено дубликатов на слиянии: {}",
            fmt_thousands(out_lines),
            fmt_size(out_bytes),
            fmt_thousands(out_dupes)
        ));

        // Временные файлы больше не нужны.
        for r in &runs {
            let _ = fs::remove_file(&r.path);
        }
        Ok(stats)
    }

    /// Параллельное слияние групп серий (фаза укрупнения). Каждая группа
    /// сливается в отдельный временный файл; одновременно работают до
    /// jobs потоков. Прогресс агрегируется атомарно.
    fn merge_groups_parallel(&self, groups: Vec<Vec<RunFile>>) -> Res<Vec<RunFile>> {
        let n_groups = groups.len();
        if n_groups == 1 {
            let grp = groups.into_iter().next().unwrap();
            let run = self.merge_group_to_run(&grp)?;
            return Ok(vec![run]);
        }
        let results: std::sync::Mutex<Vec<Option<Res<RunFile>>>> =
            std::sync::Mutex::new((0..n_groups).map(|_| None).collect());
        let groups = std::sync::Mutex::new(groups);
        let group_idx = AtomicU64::new(0);
        let jobs = self.opts.jobs.max(1).min(n_groups);
        let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
        std::thread::scope(|s| {
            for _ in 0..jobs {
                s.spawn(|| loop {
                    // Первая же ошибка останавливает раздачу работы.
                    if err.lock().unwrap().is_some() {
                        break;
                    }
                    let i = group_idx.fetch_add(1, AtomicOrdering::Relaxed) as usize;
                    if i >= n_groups {
                        break;
                    }
                    let grp = groups
                        .lock()
                        .unwrap()
                        .get_mut(i)
                        .map(std::mem::take)
                        .expect("индекс группы проверен выше");
                    match self.merge_group_to_run(&grp) {
                        Ok(run) => {
                            // Исходные серии группы можно удалить сразу.
                            for r in &grp {
                                let _ = fs::remove_file(&r.path);
                            }
                            results.lock().unwrap()[i] = Some(Ok(run));
                        }
                        Err(e) => {
                            *err.lock().unwrap() = Some(e.to_string());
                        }
                    }
                });
            }
        });
        if let Some(e) = err.into_inner().unwrap() {
            return Err(AppError::Io(e));
        }
        let mut out = Vec::with_capacity(n_groups);
        for r in results.into_inner().unwrap() {
            out.push(r.unwrap()?);
        }
        Ok(out)
    }

    /// Слить группу серий (<= fanin шт.) в один новый временный файл.
    /// Дедуп здесь НЕ выполняется — он делается на финальном слиянии
    /// (дубли из разных серий здесь всё равно встретятся только на нём).
    fn merge_group_to_run(&self, group: &[RunFile]) -> Res<RunFile> {
        let path = self.next_run_path();
        let f = File::create(&path).map_err(|e| {
            AppError::Io(format!("не удалось создать {}: {e}", path.display()))
        })?;
        let mut f = BufWriter::with_capacity(4 * 1024 * 1024, f);
        let mut bytes: u64 = 0;
        let mut dupes: u64 = 0;
        merge_sorted_runs(
            group,
            &mut |line| {
                f.write_all(line)?;
                f.write_all(b"\n")?;
                bytes += line.len() as u64 + 1;
                Ok(())
            },
            self.mode,
            self.desc,
            false,
            &self.prog,
        )?;
        let _ = &mut dupes;
        f.flush()?;
        drop(f);
        Ok(RunFile { path, bytes })
    }
}

/// Открыть все серии группы как читатели.
fn open_runs(group: &[RunFile]) -> Res<Vec<LineReader<File>>> {
    let mut readers = Vec::with_capacity(group.len());
    for r in group {
        let f = File::open(&r.path).map_err(|e| {
            AppError::Io(format!("не удалось открыть {}: {e}", r.path.display()))
        })?;
        // Буфер меньше входного: серий открывается до fanin штук
        // одновременно, и суммарная память слияния остаётся скромной.
        readers.push(LineReader::with_capacity(f, MERGE_BUF));
    }
    Ok(readers)
}

/// Элемент кучи для слияния серий: строка (копия) + индекс источника.
/// Куча — max-heap по Ord; целевой порядок инвертируется в cmp_for_heap
/// (см. chunksort), чтобы вершиной была следующая строка выхода.
struct HeapSrc {
    line: Vec<u8>,
    from: usize,
}

impl PartialEq for HeapSrc {
    fn eq(&self, o: &Self) -> bool {
        self.line == o.line && self.from == o.from
    }
}
impl Eq for HeapSrc {}
impl PartialOrd for HeapSrc {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for HeapSrc {
    fn cmp(&self, o: &Self) -> Ordering {
        cmp_for_heap(&self.line, &o.line, self.from, o.from)
    }
}

/// Ядро k-way слияния: сливает отсортированные серии и каждую строку
/// передаёт в sink (замыкание записи). Дедуп — сравнением с предыдущей
/// строкой. Используется и для укрупнения (sink пишет в файл), и для
/// финального вывода (sink пишет в PartWriter, тут же считаем статистику).
fn merge_sorted_runs<F>(
    group: &[RunFile],
    sink: &mut F,
    mode: CmpMode,
    desc: bool,
    dedup: bool,
    prog: &std::sync::Arc<Progress>,
) -> Res<(u64, u64)>
where
    F: FnMut(&[u8]) -> Res<()>,
{
    // Порядок кучи слияния задаётся через thread-local MERGE_ORDER.
    MERGE_ORDER.with(|c| *c.borrow_mut() = Some((mode, desc)));
    // Читатели + их текущие строки. Строка копируется в кучу слияния
    // (Vec): при fanin=64 и среднем строке 50-100 байт это копирование
    // дешевле, чем переусложнённые схемы с указателями.
    let mut readers = open_runs(group)?;
    let mut curs: Vec<Vec<u8>> = Vec::with_capacity(readers.len());
    let mut heap: BinaryHeap<HeapSrc> = BinaryHeap::with_capacity(readers.len());
    // Первые строки всех серий.
    for (i, r) in readers.iter_mut().enumerate() {
        let mut cur = Vec::new();
        if r.next_line(&mut cur)? {
            curs.push(cur.clone());
            heap.push(HeapSrc { line: cur, from: i });
        } else {
            curs.push(cur); // пустая серия
        }
    }
    let mut prev: Option<Vec<u8>> = None;
    let mut written: u64 = 0;
    let mut dupes: u64 = 0;
    while let Some(top) = heap.pop() {
        if dedup && prev.as_deref() == Some(top.line.as_slice()) {
            dupes += 1;
        } else {
            sink(&top.line)?;
            written += 1;
            match prev.as_mut() {
                Some(p) => {
                    p.clear();
                    p.extend_from_slice(&top.line);
                }
                None => prev = Some(top.line.clone()),
            }
        }
        prog.bytes.fetch_add(top.line.len() as u64 + 1, AtomicOrdering::Relaxed);
        prog.lines.fetch_add(1, AtomicOrdering::Relaxed);
        prog.tick();
        // Следующая строка этого источника.
        if readers[top.from].next_line(&mut curs[top.from])? {
            let line = std::mem::take(&mut curs[top.from]);
            heap.push(HeapSrc { line, from: top.from });
        }
    }
    Ok((written, dupes))
}

/// Финальное слияние серий в выходной писатель (с дедупом).
/// Возвращает (строк записано, байт записано, дубликатов удалено).
fn merge_runs_to_writer(
    group: &[RunFile],
    out: &mut PartWriter,
    mode: CmpMode,
    desc: bool,
    dedup: bool,
    prog: &std::sync::Arc<Progress>,
) -> Res<(u64, u64, u64)> {
    let mut out_bytes: u64 = 0;
    let (written, dupes) = merge_sorted_runs(
        group,
        &mut |line| {
            out.write_line(line)?;
            out_bytes += line.len() as u64 + 1;
            Ok(())
        },
        mode,
        desc,
        dedup,
        prog,
    )?;
    Ok((written, out_bytes, dupes))
}

// ============================================================================
//  app — конвейер приложения: файлы, каталоги, фильтры строк, отчёт
// ============================================================================
//  Здесь собирается всё вместе: аргументы CLI превращаются в задачу,
//  задача — в поток строк (с декодированием, trim, обрезкой), поток —
//  во внешнюю сортировку, результат — в выходной писатель с разбивкой.

/// Настройки обработки строк (общие для всех файлов задачи).
#[derive(Clone)]
pub struct LineOpts {
    /// кодировка входных файлов
    pub encoding: Encoding,
    /// обрезать строки до N СИМВОЛОВ (не байтов); None = не обрезать
    pub truncate: Option<usize>,
    /// убрать начальные/конечные пробелы
    pub trim: bool,
    /// удалять пустые строки (после trim/обрезки)
    pub skip_empty: bool,
    /// убирать строки, начинающиеся с этого префикса (например '#')
    pub skip_prefix: Option<Vec<u8>>,
}

impl LineOpts {
    /// Обработать строку: декодировать в UTF-8, обрезать, trim,
    /// вернуть false, если строка отфильтрована (пустая/префикс).
    /// Вызывается на каждую строку входа — горячая точка.
    pub fn process(&self, raw: &[u8], out: &mut Vec<u8>) -> bool {
        out.clear();
        // 1. Декодирование в UTF-8 (для UTF-8 — копирование).
        self.encoding.decode(raw, out);
        // 2. Trim по краям (до обрезки: пробелы не съедают лимит).
        if self.trim {
            trim_ascii(out);
        }
        // 3. Обрезка до N символов (Unicode code points, не байтов!).
        if let Some(max_chars) = self.truncate {
            truncate_utf8(out, max_chars);
        }
        // 4. Фильтры.
        if self.skip_empty && out.is_empty() {
            return false;
        }
        if let Some(pfx) = &self.skip_prefix {
            if out.starts_with(pfx) {
                return false;
            }
        }
        true
    }
}

/// Обрезать вектор UTF-8-байтов до max_chars символов (code points).
/// Работа по UTF-8-границам: байты-продолжения (10xxxxxx) не считаются
/// новыми символами, поэтому кириллица (2 байта) режется по символам.
fn truncate_utf8(v: &mut Vec<u8>, max_chars: usize) {
    if max_chars == 0 {
        v.clear();
        return;
    }
    let mut count = 0usize;
    for (i, &b) in v.iter().enumerate() {
        // Начало code point: не продолжение (10xxxxxx).
        if b & 0xC0 != 0x80 {
            if count == max_chars {
                v.truncate(i);
                return;
            }
            count += 1;
        }
    }
    // Символов меньше лимита — обрезать нечего.
}

/// Trim ASCII-пробельных байт по краям (in place).
fn trim_ascii(v: &mut Vec<u8>) {
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C);
    let mut start = 0;
    let mut end = v.len();
    while start < end && is_ws(v[start]) {
        start += 1;
    }
    while end > start && is_ws(v[end - 1]) {
        end -= 1;
    }
    if start > 0 || end < v.len() {
        v.drain(..start);
        v.truncate(end - start);
    }
}

/// Полные настройки задачи (после разбора CLI).
pub struct Task {
    /// режим каталога: каждый файл отдельно (false = один вход/слияние)
    pub per_file: bool,
    /// настройки обработки строк
    pub line: LineOpts,
    /// сортировать (false = --no-sort: только дедуп, исходный порядок)
    pub do_sort: bool,
    /// режим сравнения
    pub mode: CmpMode,
    /// направление сортировки
    pub desc: bool,
    /// дедуплицировать
    pub dedup: bool,
    /// лимиты разбивки выходного файла на части
    pub part_lines: Option<u64>,
    pub part_bytes: Option<u64>,
    /// каталог временных файлов (рядом с выходом или --tmp-dir)
    pub tmp_dir: PathBuf,
    /// бюджет памяти на серию, байт
    pub run_budget: usize,
    /// параллельность
    pub jobs: usize,
    /// fan-in слияния (лимит серий за один проход)
    fanin: usize,
    /// прогресс
    pub prog: std::sync::Arc<Progress>,
}

impl Task {
    /// Обработать один вход (файл или слитый каталог) в один выход.
    /// Возвращает статистику. Временные файлы удаляются.
    pub fn process_input(&self, inputs: &[PathBuf], out_path: &Path) -> Res<Stats> {
        let files: Vec<File> = inputs
            .iter()
            .map(|p| {
                File::open(p).map_err(|e| {
                    AppError::Io(format!("не удалось открыть {}: {e}", p.display()))
                })
            })
            .collect::<Res<_>>()?;
        let total_in_bytes: u64 = inputs
            .iter()
            .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .sum();

        // --- Конвейер строк: чтение -> декодирование -> фильтры ---
        let line_opts = self.line.clone();
        let mut multi = MultiReader::new(files);
        let prog = std::sync::Arc::clone(&self.prog);
        let mut raw_buf = Vec::with_capacity(256);
        let mut dec_buf = Vec::with_capacity(256);
        // Замыкание-поставщик строк для ExternalSorter::run.
        let mut fetch = |buf: &mut Vec<u8>| -> Res<bool> {
            loop {
                if !multi.next_line(&mut raw_buf)? {
                    return Ok(false);
                }
                prog.bytes.store(multi.bytes_read(), AtomicOrdering::Relaxed);
                if line_opts.process(&raw_buf, &mut dec_buf) {
                    buf.clear();
                    buf.extend_from_slice(&dec_buf);
                    return Ok(true);
                }
                // Строка отфильтрована — берём следующую.
            }
        };

        // --- Выходной писатель (с разбивкой, если задана) ---
        let mut out = PartWriter::new(
            out_path,
            self.part_lines,
            self.part_bytes,
            std::sync::Arc::clone(&self.prog),
        )?;

        // --- Сортировка (или дедуп с сохранением порядка) ---
        let stats = if self.do_sort {
            let mut sorter = ExternalSorter::new(
                self.tmp_dir.clone(),
                self.run_budget,
                self.mode,
                self.desc,
                self.dedup,
                std::sync::Arc::clone(&self.prog),
                ExtSortOpts {
                    jobs: self.jobs,
                    fanin: self.fanin,
                },
            );
            sorter.run(&mut fetch, &mut out, total_in_bytes)?
        } else {
            dedup_no_sort(&mut fetch, &mut out, &self.tmp_dir, self.run_budget, &self.prog)?
        };
        let parts = out.finish()?;
        if parts > 1 {
            self.prog.msg(&format!("результат разбит на {parts} частей"));
        }
        Ok(stats)
    }

    pub fn fanin(&self) -> usize {
        self.fanin
    }
}

/// Дедупликация БЕЗ сортировки с ограниченной памятью (--no-sort).
///
/// Хеш-таблица всех строк не поместилась бы в память на больших файлах,
/// поэтому используется приём «сортировка по ключу + восстановление
/// порядка», полностью на внешней сортировке:
///
///   Прогон 1: строки нумеруются (0,1,2,...) и внешне сортируются по
///   самой строке -> одинаковые строки оказываются рядом; оставляем
///   первую (минимальный номер) каждого значения -> файл уникальных
///   (строка, номер) с сохранением... нет, в порядке строк.
///
///   Прогон 2: полученные пары внешне сортируются по номеру -> порядок
///   вхождения восстановлен; при записи в выход номер отбрасывается.
///
/// Два прогона внешней сортировки, память ограничена бюджетом серии.
fn dedup_no_sort<F>(
    fetch: &mut F,
    out: &mut PartWriter,
    tmp_dir: &Path,
    run_budget: usize,
    prog: &std::sync::Arc<Progress>,
) -> Res<Stats>
where
    F: FnMut(&mut Vec<u8>) -> Res<bool>,
{
    // Каталог прогона: подпапка, чтобы не смешивать с сериями сортировки.
    let work = tmp_dir.join("nosort");
    let _ = fs::create_dir_all(&work);

    // ---------- Прогон 1: сортировка пар (строка, номер) ----------
    prog.phase("дедуп без сортировки: прогон 1 из 2 (группировка дубликатов)", 0, 0);
    let mut seq: u64 = 0;
    let mut fetch_pairs = |buf: &mut Vec<u8>| -> Res<bool> {
        if !fetch(buf)? {
            return Ok(false);
        }
        // Формат пары: строка \x00 номер (номер — десятичный, без
        // ведущих нулей). Сортировка Natural: равные строки идут подряд
        // (расхождение разных строк — внутри строки либо на \x00 сразу
        // после более короткой, а 0 меньше любого текстового байта),
        // а при равенстве строк номер сравнивается КАК ЧИСЛО. Поэтому
        // первая пара группы — минимальный номер = первое вхождение
        // строки (Bytes-режим сравнивал бы номера как текст: "10" < "2"
        // и порядок первого вхождения ломался, начиная с 10-й строки).
        buf.push(0);
        let seq_bytes = seq.to_string().into_bytes();
        buf.extend_from_slice(&seq_bytes);
        seq += 1;
        Ok(true)
    };
    // Сортировка пар по строке (Natural — см. выше). Дедуп на этом
    // прогоне ВЫКЛЮЧЕН — соседние пары различаются номерами, дедуп по
    // «предыдущей строке» отработает при чтении пар ниже.
    let pairs_path = work.join("pairs.sorted");
    let mut pairs_out = PartWriter::new(&pairs_path, None, None, std::sync::Arc::clone(prog))?;
    {
        let mut sorter = ExternalSorter::new(
            work.clone(),
            run_budget,
            CmpMode::Natural,
            false,
            false,
            std::sync::Arc::clone(prog),
            ExtSortOpts { jobs: 1, fanin: 64 },
        );
        sorter.run(&mut fetch_pairs, &mut pairs_out, 0)?;
    }
    let _ = pairs_out.finish()?;

    // ---------- Отбор первых вхождений -> сортировка по номеру ----------
    // Читаем отсортированные пары: группа одинаковых строк идёт подряд;
    // первая пара группы — минимальный номер (Natural-сортировка пар
    // сравнивает номера при равных строках как числа) — это и есть
    // первое вхождение строки во входе.
    prog.phase("дедуп без сортировки: отбор первых вхождений", 0, 0);
    let mut kept_lines: u64 = 0;
    let mut dupes: u64 = 0;
    let in_lines = seq;
    let uniq_path = work.join("uniq.pairs");
    let mut uniq_out = PartWriter::new(&uniq_path, None, None, std::sync::Arc::clone(prog))?;
    {
        let f = File::open(&pairs_path)
            .map_err(|e| AppError::Io(format!("не удалось открыть {}: {e}", pairs_path.display())))?;
        let mut reader = LineReader::new(f);
        let mut buf = Vec::new();
        let mut prev_line: Option<Vec<u8>> = None;
        while reader.next_line(&mut buf)? {
            // Разделить строку и номер: последний \x00.
            let Some(pos) = buf.iter().position(|&b| b == 0) else {
                continue;
            };
            let line = &buf[..pos];
            if prev_line.as_deref() == Some(line) {
                dupes += 1; // дубль: та же строка, другой номер
            } else {
                // Оставляем пару целиком (строка \x00 номер).
                uniq_out.write_line(&buf)?;
                kept_lines += 1;
                match prev_line.as_mut() {
                    Some(p) => {
                        p.clear();
                        p.extend_from_slice(line);
                    }
                    None => prev_line = Some(line.to_vec()),
                }
            }
        }
    }
    let _ = uniq_out.finish()?;
    let _ = fs::remove_file(&pairs_path);
    prog.phase_done(&format!(
        "уникальных строк: {} (дубликатов: {})",
        fmt_thousands(kept_lines),
        fmt_thousands(dupes)
    ));

    // ---------- Прогон 2: сортировка по номеру ----------
    // Формат пары разворачивается: номер \x00 строка — тогда Natural-
    // сортировка упорядочит по возрастанию номера (числа сравниваются
    // как числа, "9" < "10").
    prog.phase("дедуп без сортировки: прогон 2 из 2 (восстановление порядка)", 0, 0);
    let ord_path = work.join("ord.sorted");
    let mut ord_out = PartWriter::new(&ord_path, None, None, std::sync::Arc::clone(prog))?;
    {
        let f = File::open(&uniq_path)
            .map_err(|e| AppError::Io(format!("не удалось открыть {}: {e}", uniq_path.display())))?;
        let mut reader = LineReader::new(f);
        let mut buf: Vec<u8> = Vec::new();
        while reader.next_line(&mut buf)? {
            let Some(pos) = buf.iter().position(|&b| b == 0) else {
                continue;
            };
            let (line, num) = buf.split_at(pos);
            // num включает ведущий \x00 — пропускаем его.
            let num = &num[1..];
            let mut pair = num.to_vec();
            pair.push(0);
            pair.extend_from_slice(line);
            ord_out.write_line(&pair)?;
        }
    }
    let _ = ord_out.finish()?;
    let _ = fs::remove_file(&uniq_path);

    // Внешняя сортировка пар по номеру (Natural: числа как числа).
    let sorted_ord_path = work.join("ord.final");
    let mut sorted_out = PartWriter::new(&sorted_ord_path, None, None, std::sync::Arc::clone(prog))?;
    {
        let f = File::open(&ord_path)
            .map_err(|e| AppError::Io(format!("не удалось открыть {}: {e}", ord_path.display())))?;
        let mut reader = LineReader::new(f);
        let mut fetch3 = |buf: &mut Vec<u8>| -> Res<bool> {
            Ok(reader.next_line(buf)?)
        };
        let mut sorter2 = ExternalSorter::new(
            work.clone(),
            run_budget,
            CmpMode::Natural,
            false,
            false,
            std::sync::Arc::clone(prog),
            ExtSortOpts { jobs: 1, fanin: 64 },
        );
        sorter2.run(&mut fetch3, &mut sorted_out, 0)?;
    }
    let _ = sorted_out.finish()?;
    let _ = fs::remove_file(&ord_path);

    // ---------- Запись финального выхода: отбрасываем номера ----------
    prog.phase("дедуп без сортировки: запись результата", 0, 0);
    let mut out_lines: u64 = 0;
    let mut out_bytes: u64 = 0;
    {
        let f = File::open(&sorted_ord_path).map_err(|e| {
            AppError::Io(format!("не удалось открыть {}: {e}", sorted_ord_path.display()))
        })?;
        let mut reader = LineReader::new(f);
        let mut buf = Vec::new();
        while reader.next_line(&mut buf)? {
            let Some(pos) = buf.iter().position(|&b| b == 0) else {
                continue;
            };
            let line = &buf[pos + 1..];
            out.write_line(line)?;
            out_lines += 1;
            out_bytes += line.len() as u64 + 1;
        }
    }
    let _ = fs::remove_file(&sorted_ord_path);
    let _ = fs::remove_dir_all(&work);

    let mut stats = Stats::default();
    stats.in_lines = in_lines;
    stats.out_lines = out_lines;
    stats.out_bytes = out_bytes;
    stats.dupes_removed = dupes;
    stats.runs = 0;
    Ok(stats)
}

// ============================================================================
//  cli — разбор аргументов командной строки и справка --help
// ============================================================================

/// Разобранные аргументы командной строки.
pub struct Args {
    /// входные файлы и/или каталоги (позиционные)
    pub inputs: Vec<String>,
    /// --output / -o
    pub output: Option<String>,
    /// --merge: объединять каталог в один выход
    pub merge: bool,
    /// --suffix: суффикс имени выхода при обработке per-file
    pub suffix: String,
    /// сортировка по умолчанию вкл; --no-sort отключает
    pub do_sort: bool,
    /// --desc: по убыванию
    pub desc: bool,
    /// --numeric / -n: естественная сортировка (числа как числа)
    pub natural: bool,
    /// --ignore-case / -f: без учёта регистра (ASCII)
    pub ignore_case: bool,
    /// --unique / -u: удалить дубликаты
    pub dedup: bool,
    /// --truncate N: обрезать строки до N СИМВОЛОВ
    pub truncate: Option<usize>,
    /// --trim: убрать пробелы по краям
    pub trim: bool,
    /// --skip-empty: удалить пустые строки
    pub skip_empty: bool,
    /// --skip-prefix S: удалить строки с префиксом S (например '#')
    pub skip_prefix: Option<String>,
    /// --encoding / -e: utf8 | cp1251 | koi8-r | ibm866
    pub encoding: Encoding,
    /// --part-lines N: разбить выход на части по N строк
    pub part_lines: Option<u64>,
    /// --part-bytes N: разбить выход на части по N байт (500M и т.п.)
    pub part_bytes: Option<u64>,
    /// --memory / -m: бюджет памяти (например 1G); по умолчанию 512M
    pub memory: Option<String>,
    /// --jobs / -j: число потоков (по умолчанию — все ядра)
    pub jobs: Option<usize>,
    /// --fanin: максимум серий за проход слияния (по умолчанию 64)
    pub fanin: Option<usize>,
    /// --tmp-dir: каталог временных файлов
    pub tmp_dir: Option<String>,
    /// --quiet / -q: без прогресса, только итог
    pub quiet: bool,
    /// --verbose / -v: подробные сообщения
    pub verbose: bool,
    /// --overwrite: перезаписывать существующие выходные файлы
    pub overwrite: bool,
    /// --dry-run: только показать план, ничего не делать
    pub dry_run: bool,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            inputs: Vec::new(),
            output: None,
            merge: false,
            suffix: "_sorted".into(),
            do_sort: true,
            desc: false,
            natural: false,
            ignore_case: false,
            dedup: false,
            truncate: None,
            trim: false,
            skip_empty: false,
            skip_prefix: None,
            encoding: Encoding::Utf8,
            part_lines: None,
            part_bytes: None,
            memory: None,
            jobs: None,
            fanin: None,
            tmp_dir: None,
            quiet: false,
            verbose: false,
            overwrite: false,
            dry_run: false,
        }
    }
}

/// Полный текст справки (русский). Печатается по --help / -h и при
/// ошибке аргументов (после него — сообщение об ошибке).
pub fn help_text() -> String {
    format!(
r#"LINESORT {ver} — сортировка, дедупликация и очистка больших текстовых файлов
Память ограничена (--memory) НЕЗАВИСИМО от размера входа: внешняя сортировка
со слиянием, SIMD-поиск строк, параллельная сортировка серий.

ИСПОЛЬЗОВАНИЕ:
    linesort [ОПЦИИ] <вход...>
    <вход>          файл(ы) и/или каталог(и). Для каталога: каждый файл
                    обрабатывается отдельно; --merge объединяет все файлы
                    каталога в один выход (с общей дедупликацией).

ОСНОВНЫЕ ОПЦИИ:
  -o, --output <F>   выходной файл. Без опции: <вход>_sorted.<ext>. Для
                     каталога без --merge: суффикс добавляется каждому файлу.
      --merge        объединить все файлы каталога в один выход
                     (-o обязателен при нескольких входах/каталоге).
      --suffix <S>   суффикс имени выхода при per-file обработке
                     (по умолчанию "_sorted")
      --no-sort      не сортировать: только дедуп (--unique обязателен),
                     исходный порядок строк сохраняется
  -u, --unique       удалить дубликаты строк
  -n, --numeric      естественная сортировка: числа сравниваются как числа
                     ("a2" < "a10"); по умолчанию — строгий байтовый порядок
  -f, --ignore-case  без учёта регистра (ASCII); при равенстве — байтовый
      --desc         сортировать по убыванию (по умолчанию — по возрастанию)

ОБРАБОТКА СТРОК:
  -c, --truncate <N> обрезать строки до N СИМВОЛОВ (не байтов; кириллица
                     считается по буквам). N=0 — удалить все строки.
      --trim         убрать пробелы в начале и конце строк
      --skip-empty   удалить пустые строки (после trim/обрезки)
      --skip-prefix <S>  удалить строки, начинающиеся с S (например '#')
  -e, --encoding <E> кодировка входа: utf8 (по умолчанию) | cp1251 |
                     koi8-r | ibm866. Выход всегда UTF-8.

РАЗБИВКА ВЫХОДА:
      --part-lines <N>   разбить выход на части по N строк в каждой
      --part-bytes <N>   разбить выход на части по N байт (128M, 2G...);
                         строка никогда не разрезается посередине
    Части называются <имя>.part001.<ext>, part002 и т.д.

РЕСУРСЫ:
  -m, --memory <SZ>  бюджет памяти на сортировку (по умолчанию 512M):
                     256M, 1G, 1500M... Определяет размер серий.
  -j, --jobs <N>     число потоков сортировки (по умолчанию: все ядра)
      --fanin <N>    серий за один проход слияния (по умолчанию 64;
                     не превышает лимит открытых файлов)
      --tmp-dir <D>  каталог временных файлов (по умолчанию — рядом с
                     выходным файлом; должен вмещать ~объём входа)

ИНФОРМАЦИЯ:
  -q, --quiet        без прогресс-бара и промежуточных сообщений
      --verbose      подробный журнал (серии, проходы, имена файлов)
      --overwrite    перезаписывать существующие выходные файлы
      --dry-run      показать план обработки и выйти (ничего не писать)
  -h, --help         эта справка
  -V, --version      версия

ПРИМЕРЫ:
    # алфавитная сортировка с дедупом, отдельный выход:
    linesort -u words.txt
    # числа по возрастанию, обрезать до 40 символов:
    linesort -n --truncate 40 data.csv -o result.csv
    # каталог: каждый файл отдельно, дедуп + сортировка по убыванию:
    linesort -u --desc logs_dir/
    # весь каталог в один файл, вход в cp1251:
    linesort --merge -u -e cp1251 archive/ -o all.txt
    # большой файл, 2 ГиБ памяти, 8 потоков, разбить выход по 10 млн строк:
    linesort -u -m 2G -j 8 --part-lines 10000000 huge.txt -o out.txt
    # только дедуп, порядок как во входе:
    linesort -u --no-sort words.txt

ВЫХОДНЫЕ КОДЫ:
    0 — успех; 1 — ошибка аргументов; 2 — ошибка ввода-вывода/данных.
    Прогресс и журнал пишутся в stderr (stdout свободен для данных).
"#,
        ver = VERSION
    )
}

/// Версия утилиты.
pub const VERSION: &str = "1.0.1";

/// Разобрать аргументы (argv без имени программы).
pub fn parse_args(argv: &[String]) -> Res<Args> {
    let mut a = Args::default();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        macro_rules! val {
            ($name:literal) => {{
                i += 1;
                if i >= argv.len() {
                    return Err(AppError::Usage(format!("не задано значение для {arg}")));
                }
                argv[i].clone()
            }};
        }
        match arg {
            "-h" | "--help" => {
                return Err(AppError::Usage(help_text()));
            }
            "-V" | "--version" => {
                a.inputs.push(format!("__version__"));
                return Ok(a);
            }
            "-o" | "--output" => a.output = Some(val!("output")),
            "--merge" => a.merge = true,
            "--suffix" => a.suffix = val!("suffix"),
            "--no-sort" => a.do_sort = false,
            "-u" | "--unique" => a.dedup = true,
            "-n" | "--numeric" => a.natural = true,
            "-f" | "--ignore-case" => a.ignore_case = true,
            "--desc" => a.desc = true,
            "-c" | "--truncate" => {
                let v = val!("truncate");
                a.truncate = Some(v.parse().map_err(|_| {
                    AppError::Usage(format!("--truncate: {v:?} не целое число >= 0"))
                })?);
            }
            "--trim" => a.trim = true,
            "--skip-empty" => a.skip_empty = true,
            "--skip-prefix" => a.skip_prefix = Some(val!("skip-prefix")),
            "-e" | "--encoding" => {
                let v = val!("encoding");
                a.encoding = Encoding::parse(&v).ok_or_else(|| {
                    AppError::Usage(format!(
                        "--encoding: {v:?} — неизвестная кодировка (utf8|cp1251|koi8-r|ibm866)"
                    ))
                })?;
            }
            "--part-lines" => {
                let v = val!("part-lines");
                a.part_lines = Some(v.parse().map_err(|_| {
                    AppError::Usage(format!("--part-lines: {v:?} не целое число > 0"))
                })?);
                if a.part_lines == Some(0) {
                    return Err(AppError::Usage("--part-lines: должно быть > 0".into()));
                }
            }
            "--part-bytes" => {
                let v = val!("part-bytes");
                a.part_bytes = Some(parse_size(&v).filter(|&x| x > 0).ok_or_else(|| {
                    AppError::Usage(format!(
                        "--part-bytes: {v:?} не размер (примеры: 500M, 2G, 1048576)"
                    ))
                })?);
            }
            "-m" | "--memory" => a.memory = Some(val!("memory")),
            "-j" | "--jobs" => {
                let v = val!("jobs");
                let n: usize = v.parse().map_err(|_| {
                    AppError::Usage(format!("--jobs: {v:?} не целое число 1..=1024"))
                })?;
                if n == 0 || n > 1024 {
                    return Err(AppError::Usage("--jobs: должно быть 1..=1024".into()));
                }
                a.jobs = Some(n);
            }
            "--fanin" => {
                let v = val!("fanin");
                let n: usize = v.parse().map_err(|_| {
                    AppError::Usage(format!("--fanin: {v:?} не целое число 2..=1024"))
                })?;
                if n < 2 || n > 1024 {
                    return Err(AppError::Usage("--fanin: должно быть 2..=1024".into()));
                }
                a.fanin = Some(n);
            }
            "--tmp-dir" => a.tmp_dir = Some(val!("tmp-dir")),
            "-q" | "--quiet" => a.quiet = true,
            "-v" | "--verbose" => a.verbose = true,
            "--overwrite" => a.overwrite = true,
            "--dry-run" => a.dry_run = true,
            other => {
                if other.starts_with('-') && other.len() > 1 {
                    return Err(AppError::Usage(format!("неизвестная опция {other:?}")));
                }
                a.inputs.push(other.to_string());
            }
        }
        i += 1;
    }
    // ---------- Валидация ----------
    if a.inputs.iter().any(|x| x == "__version__") {
        // маркер --version уже обработан выше
        return Ok(a);
    }
    if a.inputs.is_empty() {
        return Err(AppError::Usage(format!(
            "не задан входной файл/каталог\n\n{}",
            help_text()
        )));
    }
    if !a.do_sort && !a.dedup {
        return Err(AppError::Usage(
            "--no-sort без --unique ничего не делает: задайте -u/--unique".into(),
        ));
    }
    if !a.do_sort && (a.desc || a.natural || a.ignore_case) {
        return Err(AppError::Usage(
            "--desc/--numeric/--ignore-case несовместимы с --no-sort".into(),
        ));
    }
    if let Some(t) = a.truncate {
        if t == 0 && !a.skip_empty {
            return Err(AppError::Usage(
                "--truncate 0 очищает все строки; добавьте --skip-empty, если это нужно".into(),
            ));
        }
    }
    Ok(a)
}

// ============================================================================
//  main — входная точка: план задачи, обход каталогов, запуск, отчёт
// ============================================================================

/// Собрать список файлов задачи из аргументов (файлы + содержимое
/// каталогов). Каталоги обходятся нерекурсивно; берутся только обычные
/// файлы; скрытые (начинаются с '.') пропускаются. Файлы сортируются
/// по имени для детерминизма.
fn collect_inputs(args: &Args) -> Res<Vec<PathBuf>> {
    let mut files = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    for spec in &args.inputs {
        let p = PathBuf::from(spec);
        if p.is_dir() {
            dirs.push(p);
        } else if p.is_file() {
            files.push(p);
        } else {
            return Err(AppError::Io(format!(
                "вход {spec:?} не существует (или недоступен)"
            )));
        }
    }
    for d in dirs {
        let mut entries: Vec<PathBuf> = Vec::new();
        for entry in fs::read_dir(&d)
            .map_err(|e| AppError::Io(format!("не удалось читать каталог {}: {e}", d.display())))?
        {
            let entry =
                entry.map_err(|e| AppError::Io(format!("каталог {}: {e}", d.display())))?;
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            // Скрытые файлы и временные файлы самой утилиты пропускаем.
            if name.starts_with('.') || name.ends_with(".part") {
                continue;
            }
            if path.is_file() {
                entries.push(path);
            }
        }
        if entries.is_empty() {
            return Err(AppError::Io(format!(
                "каталог {} не содержит обычных файлов",
                d.display()
            )));
        }
        entries.sort();
        files.extend(entries);
    }
    files.dedup();
    Ok(files)
}

/// Построить имя выходного файла для одного входа (per-file режим).
fn per_file_output(input: &Path, suffix: &str, overwrite: bool) -> Res<PathBuf> {
    let stem = input
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "out".into());
    let ext = input
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let dir = input.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let out = dir.join(format!("{stem}{suffix}{ext}"));
    if out.exists() && !overwrite {
        return Err(AppError::Io(format!(
            "выходной файл {} уже существует (используйте --overwrite)",
            out.display()
        )));
    }
    Ok(out)
}

/// Проверить, что выходной файл не совпадает ни с одним входом
/// (защита от затирания исходных данных).
fn check_output_not_input(out: &Path, inputs: &[PathBuf]) -> Res<()> {
    for i in inputs {
        if i == out {
            return Err(AppError::Usage(format!(
                "выходной файл {} совпадает со входным — отказ затирать вход",
                out.display()
            )));
        }
    }
    Ok(())
}

/// Итоговый отчёт по задаче (в stderr).
fn print_report(
    prog: &Progress,
    name: &str,
    stats: &Stats,
    inputs: &[PathBuf],
    out: &Path,
    parts: u64,
) {
    if prog.quiet() {
        return;
    }
    let in_size: u64 = inputs
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    eprintln!();
    eprintln!("┌─ отчёт: {name}");
    eprintln!("│  вход      : {} ({})", fmt_thousands(stats.in_lines), fmt_size(in_size));
    eprintln!("│  выход     : {} ({})", fmt_thousands(stats.out_lines), fmt_size(stats.out_bytes));
    if stats.dupes_removed > 0 {
        eprintln!("│  дубликатов удалено: {}", fmt_thousands(stats.dupes_removed));
    }
    if parts > 1 {
        eprintln!("│  частей    : {parts}");
    }
    eprintln!("│  серий/проходов слияния: {}/{}", stats.runs, stats.merge_passes);
    eprintln!("│  файл(ы)   : {} -> {}", inputs.len(), out.display());
    eprintln!("└─ время: {}", fmt_dur(prog.start.elapsed()));
    eprintln!();
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    // --version обрабатываем до разбора (в parse_args маркер).
    if argv.iter().any(|a| a == "-V" || a == "--version") {
        eprintln!("linesort {VERSION}");
        return;
    }
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(AppError::Usage(h)) if h.contains("LINESORT") => {
            // --help печатаем в stdout и выходим с кодом 0.
            println!("{h}");
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("linesort: {e}\n");
            eprintln!("запустите с --help для справки");
            std::process::exit(1);
        }
    };

    let code = match run(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("linesort: {e}");
            2
        }
    };
    std::process::exit(code);
}

/// Выполнить задачу по разобранным аргументам.
fn run(args: Args) -> Res<()> {
    let prog = std::sync::Arc::new(Progress::new(!args.quiet, args.verbose));
    let files = collect_inputs(&args)?;

    // Режим сравнения.
    let mode = match (args.natural, args.ignore_case) {
        (true, _) => CmpMode::Natural,
        (false, true) => CmpMode::CaseFold,
        (false, false) => CmpMode::Bytes,
    };

    // Бюджет памяти: 512 МиБ по умолчанию; арена серии = бюджет.
    let memory = args
        .memory
        .as_deref()
        .and_then(parse_size)
        .unwrap_or(512 * 1024 * 1024);
    if memory < 16 * 1024 * 1024 {
        return Err(AppError::Usage(
            "--memory: минимум 16M (внешняя сортировка требует хотя бы одну серию)".into(),
        ));
    }
    // Смещения строк внутри серии — u32, поэтому одна серия не может
    // вместить больше 4 ГиБ строк. Бюджет выше потолка не даёт выигрыша
    // и раньше приводил к переполнению (паника на файлах > 4 ГиБ).
    let arena_max = ARENA_MAX as u64;
    let effective_memory = memory.min(arena_max);
    if memory > arena_max {
        eprintln!(
            "linesort: внимание: --memory {} выше максимума одной серии (4 ГиБ); серия ограничена {}",
            fmt_size(memory),
            fmt_size(arena_max)
        );
    }
    let jobs = args.jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    let fanin = args.fanin.unwrap_or(64);

    // Формируем список задач «входы -> выход».
    // Возможны: один файл; несколько файлов без --merge (ошибка без -o?);
    // каталог per-file; каталог/файлы с --merge в один выход.
    let mut jobs_list: Vec<(Vec<PathBuf>, PathBuf)> = Vec::new();
    let multi_input = files.len() > 1 || args.merge;
    if args.merge {
        // --merge: все входы в один выход; -o обязателен.
        let out = match &args.output {
            Some(o) => PathBuf::from(o),
            None => {
                return Err(AppError::Usage(
                    "--merge: укажите выходной файл через -o".into(),
                ))
            }
        };
        check_output_not_input(&out, &files)?;
        if out.exists() && !args.overwrite && out.is_file() {
            return Err(AppError::Io(format!(
                "выходной файл {} уже существует (используйте --overwrite)",
                out.display()
            )));
        }
        jobs_list.push((files.clone(), out));
    } else if args.output.is_some() && files.len() == 1 {
        // Один вход с явным -o: обрабатываем в указанный файл.
        let out = PathBuf::from(args.output.clone().unwrap());
        check_output_not_input(&out, &files)?;
        if out.exists() && !args.overwrite && out.is_file() {
            return Err(AppError::Io(format!(
                "выходной файл {} уже существует (используйте --overwrite)",
                out.display()
            )));
        }
        jobs_list.push((files.clone(), out));
    } else if args.output.is_some() {
        // Несколько входов с -o, но без --merge: неоднозначно.
        return Err(AppError::Usage(
            "несколько входов с -o: добавьте --merge, чтобы объединить их              в один файл, либо уберите -o (каждый файл будет обработан              отдельно с суффиксом)".into(),
        ));
    } else {
        // per-file: каждый файл — отдельная задача с суффиксом.
        for f in &files {
            let out = per_file_output(f, &args.suffix, args.overwrite)?;
            check_output_not_input(&out, std::slice::from_ref(f))?;
            jobs_list.push((vec![f.clone()], out));
        }
    }

    // Каталог временных файлов: --tmp-dir, иначе рядом с выходом.
    // ВАЖНО: tmp должен вмещать ~объём входа (серии внешней сортировки).
    let tmp_root = match &args.tmp_dir {
        Some(t) => PathBuf::from(t),
        None => {
            let first_out = &jobs_list[0].1;
            let dir = first_out
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            dir.join(".linesort_tmp")
        }
    };
    fs::create_dir_all(&tmp_root)
        .map_err(|e| AppError::Io(format!("tmp-каталог {}: {e}", tmp_root.display())))?;

    // --- План задачи (dry-run показывает его) ---
    if !prog.quiet() {
        eprintln!("linesort {VERSION} — план задачи:");
        eprintln!("  вход(ы): {} файл(ов)", files.len());
        for (ins, out) in &jobs_list {
            if ins.len() == 1 {
                eprintln!("  {} -> {}", ins[0].display(), out.display());
            } else {
                eprintln!("  [{}] -> {}", ins.len(), out.display());
            }
        }
        eprintln!(
            "  сортировка: {} | дедуп: {} | память: {} | потоки: {jobs} | fanin: {fanin}",
            match mode {
                CmpMode::Bytes => "по алфавиту (байты UTF-8)".to_string(),
                CmpMode::Natural => "естественная (числа как числа)".to_string(),
                CmpMode::CaseFold => "без учёта регистра (ASCII)".to_string(),
            },
            if args.dedup { "да" } else { "нет" },
            fmt_size(memory as u64)
        );
        if args.desc {
            eprintln!("  направление: по убыванию");
        }
        if let Some(t) = args.truncate {
            eprintln!("  обрезка строк: до {t} символов");
        }
        if args.trim {
            eprintln!("  trim: да");
        }
        if args.skip_empty {
            eprintln!("  пустые строки: удаляются");
        }
        if let Some(p) = &args.skip_prefix {
            eprintln!("  пропуск строк с префиксом: {p:?}");
        }
        eprintln!("  кодировка входа: {:?} -> UTF-8", args.encoding);
        eprintln!("  tmp: {}", tmp_root.display());
        if let (Some(pl), None) = (args.part_lines, args.part_bytes) {
            eprintln!("  разбивка: по {pl} строк на часть");
        }
        if let (None, Some(pb)) = (args.part_lines, args.part_bytes) {
            eprintln!("  разбивка: по {} на часть", fmt_size(pb));
        }
        if let (Some(pl), Some(pb)) = (args.part_lines, args.part_bytes) {
            eprintln!(
                "  разбивка: по {pl} строк ИЛИ {} на часть (что раньше)",
                fmt_size(pb)
            );
        }
    }
    if args.dry_run {
        eprintln!("dry-run: запись отменена");
        return Ok(());
    }

    // --- Выполнение ---
    let line_opts = LineOpts {
        encoding: args.encoding,
        truncate: args.truncate,
        trim: args.trim,
        skip_empty: args.skip_empty,
        skip_prefix: args.skip_prefix.clone().map(|s| s.into_bytes()),
    };
    let mut total_stats = Stats::default();
    for (ins, out) in &jobs_list {
        // Уникальный подкаталог tmp для каждой задачи (параллельная
        // работа нескольких экземпляров утилиты не конфликтует).
        let task_tmp = tmp_root.join(format!(
            "{:x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        fs::create_dir_all(&task_tmp)
            .map_err(|e| AppError::Io(format!("tmp: {e}")))?;
        let task = Task {
            per_file: !multi_input,
            line: line_opts.clone(),
            do_sort: args.do_sort,
            mode,
            desc: args.desc,
            dedup: args.dedup,
            part_lines: args.part_lines,
            part_bytes: args.part_bytes,
            tmp_dir: task_tmp.clone(),
            run_budget: effective_memory as usize,
            jobs,
            fanin,
            prog: std::sync::Arc::clone(&prog),
        };
        let name = if ins.len() == 1 {
            ins[0].file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            format!("{} файлов (merge)", ins.len())
        };
        if !prog.quiet() && jobs_list.len() > 1 {
            eprintln!();
            eprintln!(">>> обрабатывается: {name}");
        }
        let t = task;
        let stats = t.process_input(ins, out)?;
        // Число частей выхода определяем по факту (part001, part002...).
        let parts = count_parts(out);
        print_report(&prog, &name, &stats, ins, out, parts);
        total_stats.in_lines += stats.in_lines;
        total_stats.out_lines += stats.out_lines;
        total_stats.dupes_removed += stats.dupes_removed;
        let _ = parts;
        let _ = fs::remove_dir_all(&task_tmp);
    }
    let _ = fs::remove_dir_all(&tmp_root);

    // Сводный отчёт, если задач было несколько.
    if jobs_list.len() > 1 && !prog.quiet() {
        eprintln!("=== сводка по {n} файлам:", n = jobs_list.len());
        eprintln!("  всего строк на входе : {}", fmt_thousands(total_stats.in_lines));
        eprintln!("  всего строк на выходе: {}", fmt_thousands(total_stats.out_lines));
        eprintln!(
            "  всего дубликатов удалено: {}",
            fmt_thousands(total_stats.dupes_removed)
        );
    }
    Ok(())
}

/// Посчитать число частей выхода: файл без разбивки = 1; с разбивкой —
/// количество файлов name.partNNN.ext.
fn count_parts(out: &Path) -> u64 {
    let stem = out
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ext = out
        .extension()
        .map(|s| format!(".{}", s.to_string_lossy()))
        .unwrap_or_default();
    let dir = out.parent().map(|p| p.to_path_buf()).unwrap_or_default();
    let dir = if dir.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        dir
    };
    // Если основной файл существует — единый выход.
    if dir.join(format!("{stem}{ext}")).is_file() {
        return 1;
    }
    let mut n = 0u64;
    let mut probe = 1u64;
    while dir.join(format!("{stem}.part{probe:03}{ext}")).is_file() {
        n = probe;
        probe += 1;
    }
    n
}

// ============================================================================
//  тесты — юнит-тесты компараторов, обрезки, кодировок, читателя
// ============================================================================
//  Запуск: cargo test (или rustc --test linesort.rs && ./linesort).
//  Функциональные (сквозные) тесты CLI — в scripts/test_linesort.sh.

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- компараторы ----------
    #[test]
    fn test_bytes_cmp() {
        assert_eq!(cmp_mode(b"a", b"b", CmpMode::Bytes), Less);
        assert_eq!(cmp_mode(b"b", b"a", CmpMode::Bytes), Greater);
        assert_eq!(cmp_mode(b"abc", b"abc", CmpMode::Bytes), Equal);
        assert_eq!(cmp_mode(b"ab", b"abc", CmpMode::Bytes), Less); // префикс
        // Кириллица UTF-8: байтовый порядок = порядок кодовых точек.
        assert_eq!(cmp_mode("абв".as_bytes(), "абг".as_bytes(), CmpMode::Bytes), Less);
    }

    #[test]
    fn test_natural_cmp() {
        assert_eq!(natural_cmp(b"a2", b"a10"), Less); // 2 < 10
        assert_eq!(natural_cmp(b"a10", b"a2"), Greater);
        assert_eq!(natural_cmp(b"file9", b"file10"), Less);
        assert_eq!(natural_cmp(b"a2b", b"a2c"), Less); // после числа
        assert_eq!(natural_cmp(b"2", b"10"), Less);
        // «07» и «7» равны как числа (7) — Equal; детерминизм
        // обеспечивается байтовым сравнением на верхнем уровне сортировки.
        assert_eq!(natural_cmp(b"07", b"7"), Equal);
        assert_eq!(natural_cmp(b"", b"x"), Less);
        assert_eq!(natural_cmp(b"x", b""), Greater);
        assert_eq!(natural_cmp(b"", b""), Equal);
        // Число против не-числа: байтовое сравнение ('2' < 'a').
        assert_eq!(natural_cmp(b"2", b"a"), Less);
    }

    #[test]
    fn test_casefold_cmp() {
        assert_eq!(cmp_mode(b"Apple", b"apple", CmpMode::CaseFold), Less);
        assert_eq!(cmp_mode(b"apple", b"Apple", CmpMode::CaseFold), Greater);
        assert_eq!(cmp_mode(b"APPLE", b"apple", CmpMode::CaseFold), Less);
        assert_eq!(cmp_mode(b"apple", b"banana", CmpMode::CaseFold), Less);
        // Регистронезависимое равенство разрешается байтовым порядком.
        assert_eq!(cmp_mode(b"abc", b"ABD", CmpMode::CaseFold), Less);
    }

    // ---------- обрезка и trim ----------
    #[test]
    fn test_truncate_utf8() {
        // Кириллица: 2 байта на символ.
        let mut v = "привет".as_bytes().to_vec(); // 6 символов, 12 байт
        truncate_utf8(&mut v, 3);
        assert_eq!(v, "при".as_bytes());
        // Меньше лимита — без изменений.
        let mut v = "ab".as_bytes().to_vec();
        truncate_utf8(&mut v, 10);
        assert_eq!(v, b"ab");
        // Точно лимит.
        let mut v = "abcd".as_bytes().to_vec();
        truncate_utf8(&mut v, 4);
        assert_eq!(v, b"abcd");
        // Смешанный текст.
        let mut v = "aбcд".as_bytes().to_vec(); // 4 символа
        truncate_utf8(&mut v, 2);
        assert_eq!(v, "aб".as_bytes());
    }

    #[test]
    fn test_trim_ascii() {
        let mut v = b"  hello  ".to_vec();
        trim_ascii(&mut v);
        assert_eq!(v, b"hello");
        let mut v = b"\t\r\n".to_vec();
        trim_ascii(&mut v);
        assert!(v.is_empty());
        let mut v = b"no-ws".to_vec();
        trim_ascii(&mut v);
        assert_eq!(v, b"no-ws");
    }

    // ---------- кодировки ----------
    #[test]
    fn test_decode_cp1251() {
        // «Привет» в cp1251: cf f0 e8 e2 e5 f2
        let src = [0xCF, 0xF0, 0xE8, 0xE2, 0xE5, 0xF2];
        let mut dst = Vec::new();
        Encoding::Cp1251.decode(&src, &mut dst);
        assert_eq!(dst, "Привет".as_bytes());
    }

    #[test]
    fn test_decode_koi8r() {
        // «Привет» в koi8-r: f0 d2 c9 d7 c5 d4
        let src = [0xF0, 0xD2, 0xC9, 0xD7, 0xC5, 0xD4];
        let mut dst = Vec::new();
        Encoding::Koi8r.decode(&src, &mut dst);
        assert_eq!(dst, "Привет".as_bytes());
    }

    #[test]
    fn test_decode_ibm866() {
        // «привет» в cp866: af e0 a8 a2 a5 e2
        let src = [0xAF, 0xE0, 0xA8, 0xA2, 0xA5, 0xE2];
        let mut dst = Vec::new();
        Encoding::Ibm866.decode(&src, &mut dst);
        assert_eq!(dst, "привет".as_bytes());
    }

    #[test]
    fn test_decode_utf8_passthrough() {
        let src = "Привет".as_bytes();
        let mut dst = Vec::new();
        Encoding::Utf8.decode(src, &mut dst);
        assert_eq!(dst, src);
    }

    // ---------- parse_size ----------
    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("512"), Some(512));
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("100M"), Some(100 * 1024 * 1024));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
        assert_eq!(parse_size("1G64M"), Some((1 << 30) + (64 << 20)));
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size(""), None);
    }

    // ---------- читатель строк ----------
    #[test]
    fn test_line_reader_basic() {
        let data = b"one\ntwo\nthree";
        let mut r = LineReader::new(&data[..]);
        let mut buf = Vec::new();
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"one");
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"two");
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"three");
        assert!(!r.next_line(&mut buf).unwrap());
    }

    #[test]
    fn test_line_reader_crlf_and_empty() {
        let data = b"a\r\n\r\nb\n";
        let mut r = LineReader::new(&data[..]);
        let mut buf = Vec::new();
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"a");
        assert!(r.next_line(&mut buf).unwrap());
        assert!(buf.is_empty()); // пустая строка
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"b");
        assert!(!r.next_line(&mut buf).unwrap());
    }

    #[test]
    fn test_line_reader_long_line() {
        // Строка длиннее буфера чтения (8 МиБ) — должна собраться целиком.
        let long = vec![b'x'; 9 * 1024 * 1024];
        let mut data = long.clone();
        data.push(b'\n');
        data.extend_from_slice(b"tail\n");
        let mut r = LineReader::new(&data[..]);
        let mut buf = Vec::new();
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf.len(), 9 * 1024 * 1024);
        assert!(r.next_line(&mut buf).unwrap());
        assert_eq!(buf, b"tail");
    }

    // ---------- LineOpts ----------
    #[test]
    fn test_line_opts_process() {
        let lo = LineOpts {
            encoding: Encoding::Utf8,
            truncate: Some(5),
            trim: true,
            skip_empty: true,
            skip_prefix: Some(b"#".to_vec()),
        };
        let mut out = Vec::new();
        // Порядок: trim -> truncate: «  приветмир  » -> «приветмир» -> «приве».
        assert!(lo.process("  приветмир  ".as_bytes(), &mut out));
        assert_eq!(out, "приве".as_bytes());
        // Пустая строка после trim отбрасывается.
        assert!(!lo.process(b"   ", &mut out));
        // Префикс-фильтр.
        assert!(!lo.process("# комментарий".as_bytes(), &mut out));
    }

    // ---------- Arena ----------
    #[test]
    fn test_arena_push_get() {
        let mut a = Arena::new(1024);
        let r1 = a.push(b"hello");
        let r2 = a.push("мир".as_bytes());
        assert_eq!(a.get(r1), b"hello");
        assert_eq!(a.get(r2), "мир".as_bytes());
        assert_eq!(a.used(), 5 + 6);
    }

    // Потолок серии: бюджет пользователя не раздвигает u32-предел
    // смещений LineRef (регрессия паники на файлах > 4 ГиБ, v1.0.1).
    #[test]
    fn test_arena_cap_capped_to_arena_max() {
        let a = Arena::new(64 * 1024 * 1024 * 1024); // бюджет 64 ГиБ
        assert_eq!(a.cap(), (u32::MAX as usize) - 64 * 1024);
        let b = Arena::new(256 * 1024 * 1024); // бюджет ниже потолка
        assert_eq!(b.cap(), 256 * 1024 * 1024);
    }

    // ---------- SIMD ----------
    #[test]
    fn test_simd_find() {
        assert_eq!(simd::find_byte(b"abc\nxyz\n", b'\n'), Some(3));
        assert_eq!(simd::find_byte(b"no newline", b'\n'), None);
        // Буфер больше 32 байт (проверяет AVX2-путь и хвост).
        let big: Vec<u8> = (0..=100u8).collect(); // 101 элемент, есть 0x64
        assert_eq!(simd::find_byte(&big, b'\n'), Some(10)); // 0x0A
        assert_eq!(simd::find_byte(&big, b'd'), Some(100)); // 0x64 = 100
    }

    // ---------- chunksort: слияние долей ----------
    #[test]
    fn test_chunk_merger() {
        // Строим арену с несортированными строками, сортируем в 3 доли.
        let strings: Vec<&[u8]> = vec![b"delta", b"alpha", b"echo", b"bravo", b"charlie", b"foxtrot"];
        let mut arena = Vec::new();
        let mut refs = Vec::new();
        for s in &strings {
            let off = arena.len() as u32;
            arena.extend_from_slice(s);
            refs.push(LineRef { off, len: s.len() as u32 });
        }
        let arena = std::sync::Arc::new(arena);
        let chunks = sort_refs_parallel(arena.clone(), refs, CmpMode::Bytes, false, 3);
        let mut m = ChunkMerger::new(chunks, &arena, CmpMode::Bytes, false);
        let mut got = Vec::new();
        while let Some(l) = m.next() {
            got.push(String::from_utf8_lossy(l).into_owned());
        }
        assert_eq!(got, vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"]);
    }

    // ---------- сквозной тест внешней сортировки ----------
    #[test]
    fn test_external_sort_roundtrip() {
        let dir = std::env::temp_dir().join(format!("linesort_test_{:x}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // Вход: дубликаты, неупорядоченные строки.
        let lines = vec!["бета", "альфа", "бета", "гамма", "альфа", "10", "2"];
        let mut input = lines.join("\n").into_bytes();
        input.push(b'\n');
        let in_path = dir.join("in.txt");
        fs::write(&in_path, &input).unwrap();
        // Поток строк.
        let mut pos = 0usize;
        let data = input.clone();
        let mut fetch = |buf: &mut Vec<u8>| -> Res<bool> {
            if pos >= data.len() {
                return Ok(false);
            }
            let end = data[pos..].iter().position(|&b| b == b'\n').unwrap() + pos;
            buf.clear();
            buf.extend_from_slice(&data[pos..end]);
            pos = end + 1;
            Ok(true)
        };
        let prog = std::sync::Arc::new(Progress::new(false, false));
        let mut sorter = ExternalSorter::new(
            dir.clone(),
            1024, // крошечный бюджет: обязательно несколько серий
            CmpMode::Bytes,
            false,
            true,
            prog.clone(),
            ExtSortOpts { jobs: 2, fanin: 2 }, // fanin=2: форсируем укрупнение
        );
        let out_path = dir.join("out.txt");
        let mut out = PartWriter::new(&out_path, None, None, prog.clone()).unwrap();
        let stats = sorter.run(&mut fetch, &mut out, input.len() as u64).unwrap();
        out.finish().unwrap();
        let result = fs::read_to_string(&out_path).unwrap();
        assert_eq!(result, "10\n2\nальфа\nбета\nгамма\n");
        assert_eq!(stats.in_lines, 7);
        assert_eq!(stats.out_lines, 5);
        assert_eq!(stats.dupes_removed, 2);
        let _ = fs::remove_dir_all(&dir);
    }

    // ---------- дедуп без сортировки: порядок первого вхождения ----------
    // Регрессия v1.0.1: прогон 1 сортировал пары «строка\x00номер» в
    // режиме Bytes и при равных строках сравнивал номера как текст
    // («10» < «2») — «первая пара группы» была не первым вхождением,
    // порядок строк в выходе ломался, начиная с 10-й строки.
    #[test]
    fn test_dedup_no_sort_order() {
        let dir = std::env::temp_dir().join(format!("linesort_test_ns_{:x}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        // 12 строк: дубликаты с номерами вхождений разной длины (>= 10).
        let lines: Vec<&str> = vec![
            "first", "dup", "filler1", "dup", "filler2", "dup", "third",
            "filler3", "dup", "filler4", "filler5", "dup",
        ];
        let mut input = lines.join("\n").into_bytes();
        input.push(b'\n');
        let in_path = dir.join("in.txt");
        fs::write(&in_path, &input).unwrap();
        let prog = std::sync::Arc::new(Progress::new(false, false));
        let out_path = dir.join("out.txt");
        let mut fetch_pos = 0usize;
        let data = input.clone();
        let mut fetch = |buf: &mut Vec<u8>| -> Res<bool> {
            if fetch_pos >= data.len() {
                return Ok(false);
            }
            let end = data[fetch_pos..].iter().position(|&b| b == b'\n').unwrap() + fetch_pos;
            buf.clear();
            buf.extend_from_slice(&data[fetch_pos..end]);
            fetch_pos = end + 1;
            Ok(true)
        };
        let mut out = PartWriter::new(&out_path, None, None, prog.clone()).unwrap();
        dedup_no_sort(&mut fetch, &mut out, &dir, 1024, &prog).unwrap();
        out.finish().unwrap();
        let result = fs::read_to_string(&out_path).unwrap();
        // Ожидание: порядок первого вхождения, дубликаты удалены.
        assert_eq!(result, "first\ndup\nfiller1\nfiller2\nthird\nfiller3\nfiller4\nfiller5\n");
        let _ = fs::remove_dir_all(&dir);
    }
}
