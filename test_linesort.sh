#!/usr/bin/env bash
# ============================================================================
#  Функциональные тесты linesort CLI.
#  Запуск: bash test_linesort.sh [путь-к-бинарнику; по умолчанию target/release/linesort]
#  Каждый тест: подготовка данных -> запуск -> проверка результата.
#  Выход: 0 = все прошли; 1 = есть провалы (список в конце).
# ============================================================================
set -u
BIN="${1:-target/release/linesort}"
# python3 везде, кроме Windows (там в Git Bash обычно только python)
PYTHON="$(command -v python3 || command -v python)"
WORK="$(mktemp -d /tmp/linesort_test.XXXXXX)"
PASS=0
FAIL=0
FAILED_NAMES=()

# Проверка: файл $1 в точности равен stdin.
check_eq() {
    local name="$1" file="$2" expected="$3"
    if [[ "$(cat "$file")" == "$expected" ]]; then
        PASS=$((PASS+1)); echo "  ok   $name"
    else
        FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
        echo "  FAIL $name"
        echo "    ожидалось: $(echo "$expected" | head -5)"
        echo "    получено : $(head -5 "$file")"
    fi
}

# Проверка: условие $2 истинно.
check() {
    local name="$1"; shift
    if "$@"; then
        PASS=$((PASS+1)); echo "  ok   $name"
    else
        FAIL=$((FAIL+1)); FAILED_NAMES+=("$name")
        echo "  FAIL $name"
    fi
}

echo "== linesort функциональные тесты (бинарник: $BIN) =="

# ---------- 1. Простая алфавитная сортировка ----------
D="$WORK/t1"; mkdir -p "$D"
printf 'банан\nяблоко\nапельсин\nкиви\n' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q || echo "FAIL запуск t1"
check_eq "сортировка по алфавиту" "$D/out.txt" "$(printf 'апельсин\nбанан\nкиви\nяблоко')"

# ---------- 2. Дедупликация ----------
D="$WORK/t2"; mkdir -p "$D"
printf 'a\nb\na\nc\nb\na\n' > "$D/in.txt"
"$BIN" -u "$D/in.txt" -o "$D/out.txt" -q
check_eq "дедупликация -u" "$D/out.txt" "$(printf 'a\nb\nc')"

# ---------- 3. Числовая (естественная) сортировка ----------
D="$WORK/t3"; mkdir -p "$D"
printf 'file10\nfile2\nfile1\nfile20\nfile3\n' > "$D/in.txt"
"$BIN" -n "$D/in.txt" -o "$D/out.txt" -q
check_eq "естественная сортировка" "$D/out.txt" "$(printf 'file1\nfile2\nfile3\nfile10\nfile20')"

# ---------- 4. Обычная (байтовая) сортировка тех же данных ----------
D="$WORK/t4"; mkdir -p "$D"
printf 'file10\nfile2\nfile1\nfile20\nfile3\n' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q
check_eq "байтовая сортировка (10 < 2)" "$D/out.txt" "$(printf 'file1\nfile10\nfile2\nfile20\nfile3')"

# ---------- 5. Сортировка по убыванию ----------
D="$WORK/t5"; mkdir -p "$D"
printf '1\n3\n2\n' > "$D/in.txt"
"$BIN" --desc "$D/in.txt" -o "$D/out.txt" -q
check_eq "сортировка по убыванию" "$D/out.txt" "$(printf '3\n2\n1')"

# ---------- 6. Обрезка строк по символам ----------
D="$WORK/t6"; mkdir -p "$D"
printf 'привет\nда\nабракадабра\n' > "$D/in.txt"
"$BIN" --truncate 3 "$D/in.txt" -o "$D/out.txt" -q
check_eq "обрезка до 3 символов" "$D/out.txt" "$(printf 'абр\nда\nпри')"

# ---------- 7. Обрезка + trim вместе ----------
D="$WORK/t7"; mkdir -p "$D"
printf '  длинная строка  \nкороткая\n' > "$D/in.txt"
"$BIN" --trim --truncate 7 "$D/in.txt" -o "$D/out.txt" -q
check_eq "trim затем truncate 7" "$D/out.txt" "$(printf 'длинная\nкоротка')"

# ---------- 8. Пропуск пустых строк ----------
D="$WORK/t8"; mkdir -p "$D"
printf 'a\n\n\nb\n' > "$D/in.txt"
"$BIN" --skip-empty "$D/in.txt" -o "$D/out.txt" -q
check_eq "пропуск пустых строк" "$D/out.txt" "$(printf 'a\nb')"

# ---------- 9. Пропуск по префиксу ----------
D="$WORK/t9"; mkdir -p "$D"
printf '# коммент\nданные\n# ещё\nстрока\n' > "$D/in.txt"
"$BIN" --skip-prefix '#' "$D/in.txt" -o "$D/out.txt" -q
check_eq "пропуск строк с #" "$D/out.txt" "$(printf 'данные\nстрока')"

# ---------- 10. Разбивка по строкам ----------
D="$WORK/t10"; mkdir -p "$D"
printf 'a\nb\nc\nd\ne\n' > "$D/in.txt"
"$BIN" --part-lines 2 "$D/in.txt" -o "$D/out.txt" -q
check_eq "часть 1 (2 строки)" "$D/out.part001.txt" "$(printf 'a\nb')"
check_eq "часть 2 (2 строки)" "$D/out.part002.txt" "$(printf 'c\nd')"
check_eq "часть 3 (1 строка)" "$D/out.part003.txt" "$(printf 'e')"

# ---------- 11. Разбивка по байтам ----------
D="$WORK/t11"; mkdir -p "$D"
printf 'aaaa\nbb\ncccc\n' > "$D/in.txt"   # 5+3+5 = 13 байт
"$BIN" --part-bytes 10 "$D/in.txt" -o "$D/out.txt" -q
check_eq "часть 1 (<=10 байт)" "$D/out.part001.txt" "$(printf 'aaaa\nbb')"
check_eq "часть 2 (остаток)" "$D/out.part002.txt" "$(printf 'cccc')"

# ---------- 12. Каталог: каждый файл отдельно ----------
D="$WORK/t12"; mkdir -p "$D/dir"
printf 'b\na\n' > "$D/dir/one.txt"
printf 'd\nc\n' > "$D/dir/two.log"
"$BIN" "$D/dir" -q
check_eq "per-file: one" "$D/dir/one_sorted.txt" "$(printf 'a\nb')"
check_eq "per-file: two" "$D/dir/two_sorted.log" "$(printf 'c\nd')"

# ---------- 13. Каталог: --merge в один файл ----------
D="$WORK/t13"; mkdir -p "$D/dir"
printf 'b\na\n' > "$D/dir/x.txt"
printf 'a\nc\n' > "$D/dir/y.txt"
"$BIN" --merge -u "$D/dir" -o "$D/all.txt" -q
check_eq "merge + дедуп" "$D/all.txt" "$(printf 'a\nb\nc')"

# ---------- 14. Кодировка cp1251 ----------
D="$WORK/t14"; mkdir -p "$D"
printf '\xcf\xf0\xe8\xe2\xe5\xf2\n\xcc\xe8\xf0\n' > "$D/in.txt"   # Привет, Мир
"$BIN" -e cp1251 "$D/in.txt" -o "$D/out.txt" -q
check_eq "cp1251 -> UTF-8 сортировка" "$D/out.txt" "$(printf 'Мир\nПривет')"

# ---------- 15. Кодировка koi8-r ----------
D="$WORK/t15"; mkdir -p "$D"
printf '\xc2\xd7\xc7\n\xc1\xc2\xd7\n' > "$D/in.txt"   # бвг, абв (koi8-r маленькие)
"$BIN" -e koi8-r "$D/in.txt" -o "$D/out.txt" -q
check_eq "koi8-r -> UTF-8" "$D/out.txt" "$(printf 'абв\nбвг')"

# ---------- 16. Кодировка cp866 ----------
D="$WORK/t16"; mkdir -p "$D"
printf '\xaf\xe0\xa8\xa2\xa5\xe2\n\x8c\xa8\xe0\n' > "$D/in.txt"  # привет, Мир
"$BIN" -e ibm866 "$D/in.txt" -o "$D/out.txt" -q
check_eq "cp866 -> UTF-8" "$D/out.txt" "$(printf 'Мир\nпривет')"

# ---------- 17. --no-sort: дедуп с сохранением порядка ----------
D="$WORK/t17"; mkdir -p "$D"
printf 'первый\nвторой\nпервый\nтретий\nвторой\n' > "$D/in.txt"
"$BIN" -u --no-sort "$D/in.txt" -o "$D/out.txt" -q
check_eq "no-sort дедуп (порядок сохранён)" "$D/out.txt" "$(printf 'первый\nвторой\nтретий')"

# ---------- 18. Регистронезависимая сортировка ----------
D="$WORK/t18"; mkdir -p "$D"
printf 'Banana\napple\nCherry\n' > "$D/in.txt"
"$BIN" -f "$D/in.txt" -o "$D/out.txt" -q
check_eq "ignore-case сортировка" "$D/out.txt" "$(printf 'apple\nBanana\nCherry')"

# ---------- 19. CRLF-вход ----------
D="$WORK/t19"; mkdir -p "$D"
printf 'b\r\na\r\n' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q
check_eq "CRLF обрабатывается" "$D/out.txt" "$(printf 'a\nb')"

# ---------- 20. Пустой входной файл ----------
D="$WORK/t20"; mkdir -p "$D"
: > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q
check_eq "пустой вход -> пустой выход" "$D/out.txt" ""

# ---------- 21. Последняя строка без \n ----------
D="$WORK/t21"; mkdir -p "$D"
printf 'b\na' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q
check_eq "нет \n в конце" "$D/out.txt" "$(printf 'a\nb')"

# ---------- 22. Без --overwrite существующий выход не трогаем ----------
D="$WORK/t22"; mkdir -p "$D"
printf 'a\n' > "$D/in.txt"; printf 'НЕ ТРОГАТЬ\n' > "$D/out.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q 2>/dev/null
check_eq "выход не перезаписан" "$D/out.txt" "НЕ ТРОГАТЬ"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q --overwrite 2>/dev/null
check_eq "--overwrite работает" "$D/out.txt" "a"

# ---------- 23. Выход не может совпадать со входом ----------
D="$WORK/t23"; mkdir -p "$D"
printf 'a\n' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/in.txt" -q 2>/dev/null && RC=0 || RC=$?
check "вход==выход отклонён" test "$RC" -ne 0

# ---------- 24. --dry-run ничего не пишет ----------
D="$WORK/t24"; mkdir -p "$D"
printf 'b\na\n' > "$D/in.txt"
"$BIN" "$D/in.txt" -o "$D/out.txt" -q --dry-run >/dev/null 2>&1
check "dry-run не создаёт выход" test ! -e "$D/out.txt"

# ---------- 25. --help и --version ----------
"$BIN" --help >/dev/null 2>&1
check "--help код 0" test $? -eq 0
"$BIN" --version >/dev/null 2>&1
check "--version код 0" test $? -eq 0
H="$("$BIN" --help 2>&1)"
check "help содержит примеры" grep -q "ПРИМЕРЫ" <<<"$H"
check "help содержит --part-bytes" grep -q "part-bytes" <<<"$H"

# ---------- 26. Разные флаги вместе: дедуп+обрезка+убывание+разбивка ----------
D="$WORK/t26"; mkdir -p "$D"
printf 'zzzzzzzz\ndata\ndata\naa\n' > "$D/in.txt"
"$BIN" -u --truncate 4 --desc --part-lines 2 "$D/in.txt" -o "$D/out.txt" -q
check_eq "комбо: часть1" "$D/out.part001.txt" "$(printf 'zzzz\ndata')"
check_eq "комбо: часть2" "$D/out.part002.txt" "aa"

# ---------- 27. Многосерийный режим: маленький --memory ----------
D="$WORK/t27"; mkdir -p "$D"
"$PYTHON" -c "
import random
random.seed(42)
words = ['{}{:05d}'.format('абвгд', i) for i in range(5000)]
words *= 2  # дубликаты
random.shuffle(words)
print('\n'.join(words))
" > "$D/in.txt"
"$BIN" -u -m 16M --part-lines 3000 "$D/in.txt" -o "$D/out.txt" -q
N=$(cat "$D"/out.part*.txt | wc -l)
check "многосерийный: 5000 уникальных" test "$N" -eq 5000
S=$(cat "$D"/out.part*.txt | sort -c 2>/dev/null; echo $?)
check "многосерийный: выход отсортирован" test "$S" -eq 0

# ---------- 28. Merge каталога в одной кодировке koi8-r ----------
D="$WORK/t28"; mkdir -p "$D/dir"
printf '\xc2\xd7\n' > "$D/dir/koi1.txt"      # бв
printf '\xc1\xc2\xd7\n' > "$D/dir/koi2.txt" # абв
"$BIN" --merge -u -e koi8-r "$D/dir" -o "$D/out.txt" -q
check_eq "merge koi8-r каталога" "$D/out.txt" "$(printf 'абв\nбв')"

echo
echo "== итого: $PASS ok, $FAIL fail =="
if [[ $FAIL -gt 0 ]]; then
    echo "провалены:"; printf '  %s\n' "${FAILED_NAMES[@]}"
    exit 1
fi
rm -rf "$WORK"
exit 0
