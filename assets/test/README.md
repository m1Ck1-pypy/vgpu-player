# Тестовые медиафайлы

Файлы из этого каталога в git не попадают (см. `.gitignore`): они большие и
полностью воспроизводимы. Ниже — как их получить.

## Основной клип для этапов 1–6

Видео со счётчиком кадров и писком раз в секунду: по нему проверяются
аудио-часы и A/V-синхронизация.

```bash
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 \
       -f lavfi -i "sine=frequency=1000:beep_factor=4" \
       -t 30 -c:v libx264 -pix_fmt yuv420p -c:a aac assets/test/sync_test.mp4
```

То же в WebM (VP9 + Opus) — проверка другого демуксера. `-deadline
realtime -cpu-used 8` ускоряют кодирование, качество для тестов не важно:

```bash
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i "sine=frequency=1000" \
       -t 30 -c:v libvpx-vp9 -deadline realtime -cpu-used 8 -c:a libopus assets/test/sync_test.webm
```

## Только звук и только видео

Нужны для проверки запасных часов, когда ведущего аудио-потока нет.

```bash
ffmpeg -f lavfi -i "sine=frequency=1000" -t 10 -c:a aac assets/test/audio_only.m4a
ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -t 10 \
       -c:v libx264 -pix_fmt yuv420p assets/test/video_only.mp4
```

## AV1 (нужен `libdav1d`)

Встроенный в FFmpeg декодер `av1` умеет только аппаратное декодирование,
поэтому проект собирает FFmpeg с `libdav1d`:

```bash
.tools/vcpkg/vcpkg install "ffmpeg[avcodec,avdevice,avfilter,avformat,swresample,swscale,dav1d]" --triplet x64-windows --recurse
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/
cp .tools/vcpkg/installed/x64-windows/bin/*.dll target/debug/deps/
```

Короткий AV1-клип для теста (2 секунды, без перекодирования):

```bash
ffmpeg -hide_banner -loglevel error -y -i assets/test/test_10min.webm \
       -t 2 -c copy assets/test/av1_test.webm
```

Если нужен файл с нуля (при наличии AV1-энкодера):

```bash
ffmpeg -f lavfi -i testsrc2=size=640x360:rate=30 -t 2 \
       -c:v libsvtav1 assets/test/av1_test.webm
```

## «Плохие» файлы (этап 7)

```bash
: > assets/test/empty.mp4
head -c 100000 assets/test/sync_test.mp4 > assets/test/truncated.mp4
```

Несуществующий файл отдельно создавать не нужно: тест берёт заведомо
отсутствующий путь.

## Замер производительности (этап 8)

10 минут 1080p30: по этому файлу проверяется, что память не растёт и кадры
не пропускаются. Звук — тишина (`anullsrc`), чтобы десятиминутный прогон не
шумел; для декодера это та же нагрузка, что и музыка.

```bash
ffmpeg -hide_banner -loglevel error -y \
       -f lavfi -i testsrc2=size=1920x1080:rate=30 \
       -f lavfi -i anullsrc=channel_layout=stereo:sample_rate=44100 \
       -t 600 -c:v libx264 -preset ultrafast -crf 28 -pix_fmt yuv420p -g 60 \
       -c:a aac -b:a 128k assets/test/bench_10min_1080p.mp4
```

Замеры:

```bash
# пропускная способность декодера и конвертации в BGRA (без UI и звука)
time target/debug/player-cli.exe assets/test/bench_10min_1080p.mp4 --decode-frames 18000

# воспроизведение в реальном времени: RSS и CPU процесса, пропуски в итоге
powershell -NoProfile -Command "Get-Process player-cli | Select-Object WorkingSet64,CPU"
```

## Проверка после генерации

```bash
ffprobe -hide_banner assets/test/sync_test.mp4
```
