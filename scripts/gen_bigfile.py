#!/usr/bin/env python3
"""Генератор большого тестового файла для linesort.

Строки трёх сортов (смешаны):
  * слова: случайные русские/латинские слова с шумом;
  * числа: целые в диапазоне 1..10^7 (много дубликатов);
  * CSV-подобные: id,имя,число.
Доля дубликатов ~40%. Пишем порциями по 16 МиБ, чтобы не жрать память.
"""
import os
import random
import sys

OUT = sys.argv[1] if len(sys.argv) > 1 else "/tmp/big/in.txt"
TARGET_BYTES = int(sys.argv[2]) if len(sys.argv) > 2 else 1_500_000_000
SEED = int(sys.argv[3]) if len(sys.argv) > 3 else 2026

random.seed(SEED)
os.makedirs(os.path.dirname(OUT), exist_ok=True)

LATIN = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta",
         "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron"]
CYR = ["альфа", "бета", "гамма", "дельта", "эпсилон", "дзета", "эта",
       "тета", "йота", "каппа", "лямбда", "мю", "ню", "кси", "омикрон"]

def rand_word():
    w = random.choice(LATIN + CYR)
    n = random.randint(1, 9999)
    suf = random.choice(["", f"-{n}", f"_{n}", f".{n}"])
    return w + suf

def rand_line():
    r = random.random()
    if r < 0.35:
        return str(random.randint(1, 10_000_000))
    if r < 0.70:
        return rand_word()
    return f"{random.randint(1,100000)},{rand_word()},{random.randint(0,9999)}"

written = 0
buf = []
buf_bytes = 0
with open(OUT, "w", encoding="utf-8") as f:
    while written < TARGET_BYTES:
        line = rand_line()
        # 40% строк — повторы недавно выданных.
        if random.random() < 0.40 and buf:
            line = buf[random.randrange(len(buf))]
        else:
            if len(buf) > 512:
                buf.pop(0)
            buf.append(line)
        f.write(line + "\n")
        written += len(line) + 1
        buf_bytes = 0
print(f"готово: {OUT}, {written:,} байт")
