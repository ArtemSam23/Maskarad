#!/usr/bin/env bash
# Прогон эталонного эндпоинта организаторов, чтобы увидеть точный вид эталонной маски
# по каждому типу ПДн (метрика проверки — Левенштейн против эталона, поэтому формат важен).
# Использование: bash scripts/probe_reference.sh [URL]
set -u
URL="${1:-https://process-test.holydev.space/process}"

probes=(
  "Клиент Иванов Иван Иванович, паспорт 4509 123456"
  "иванов иван иванович родился 12.05.1987 в г. Москва"
  "Дата рождения: 5 марта 1990 года, место рождения: город Санкт-Петербург"
  "Паспорт серия 45 09 номер 123456, выдан ОВД района Хамовники г. Москвы 15.06.2010, код подразделения 770-001"
  "Гражданство: Российская Федерация. Гражданин России Петров Пётр Петрович"
  "Водительское удостоверение 77 АА 123456, выдано 2015.03.22"
  "Адрес: 101000, Россия, г. Москва, ул. Тверская, д. 7, корп. 2, кв. 12"
  "Email: Ivanov.Ivan@Mail.ru, телефон +7 (925) 123-45-67, ИНН 500100732259"
  "Карта 4276 3800 1234 5678, CVV 123, пин-код 4321, держатель IVAN IVANOV"
  "Поэт Александр Пушкин родился 6 июня 1799 года в Москве"
  "Отделение банка по адресу г. Москва, ул. Каланчёвская, д. 27 работает до 20:00"
  "Пин-код 1234"
  "тестовая строка"
)

for p in "${probes[@]}"; do
  id="$(date +%s%N)-$RANDOM"
  body="$(printf '%s' "$p" | python3 -c 'import json,sys;print(json.dumps({"payload":sys.stdin.read(),"payload_id":sys.argv[1]},ensure_ascii=False))' "$id" 2>/dev/null \
      || printf '{"payload":"%s","payload_id":"%s"}' "$p" "$id")"
  echo "=== IN : $p"
  printf "    MASK  : "; curl -sS -k -m 15 -X POST "$URL" -H "Content-Type: application/json" -d "$body"; echo
  masked="$(curl -sS -k -m 15 -X POST "$URL" -H "Content-Type: application/json" -d "$body" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"])' 2>/dev/null)"
  body2="$(printf '%s' "$masked" | python3 -c 'import json,sys;print(json.dumps({"payload":sys.stdin.read(),"payload_id":sys.argv[1]},ensure_ascii=False))' "$id" 2>/dev/null)"
  printf "    DEMASK: "; curl -sS -k -m 15 -X POST "$URL" -H "Content-Type: application/json" -d "$body2"; echo
done
