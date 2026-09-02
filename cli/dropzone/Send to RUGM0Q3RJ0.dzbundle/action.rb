# Dropzone Action Info
# Name: Send to RUGM0Q3RJ0
# Description: Sends dropped files or text to RUGM0Q3RJ0 through LocalSend CLI. A click sends the clipboard image or text.
# Handles: Files, Text
# Creator: Alexander Gordeev
# URL: https://github.com/localsend/localsend
# Events: Clicked, Dragged
# SkipConfig: Yes
# RunsSandboxed: No
# Version: 1.1
# MinDropzoneVersion: 5.0
# UniqueID: 837421905

require 'open3'
require 'tempfile'

CLI_PATH = File.expand_path('~/.local/bin/localsend-cli').freeze
CLIPBOARD_IMAGE_SCRIPT = <<~APPLESCRIPT.freeze
  on run argv
    try
      set image_data to the clipboard as «class PNGf»
    on error error_message number error_number
      if error_number is -1700 then return "NO_IMAGE"
      error error_message number error_number
    end try

    set output_file to POSIX file (item 1 of argv)
    set file_handle to open for access output_file with write permission
    try
      set eof file_handle to 0
      write image_data to file_handle
      close access file_handle
    on error error_message number error_number
      try
        close access file_handle
      end try
      error error_message number error_number
    end try
    return "IMAGE"
  end run
APPLESCRIPT

def run_cli(arguments)
  stdout, stderr, status = Open3.capture3(CLI_PATH, *arguments)
  return nil if status.success?

  detail = stderr.strip
  detail = stdout.strip if detail.empty?
  detail.empty? ? "localsend-cli завершился с кодом #{status.exitstatus}" : detail
rescue Errno::ENOENT
  "Программа не найдена: #{CLI_PATH}"
rescue Errno::EACCES
  "Нет разрешения на запуск: #{CLI_PATH}"
end

def send_text(text)
  if text.nil? || text.empty?
    $dz.fail('Нет текста для отправки')
    return
  end

  $dz.begin('Отправка текста на RUGM0Q3RJ0...')
  error = run_cli(['--text', text])
  if error
    $dz.fail(error)
    return
  end

  $dz.finish('Текст отправлен на RUGM0Q3RJ0')
  $dz.url(false)
end

def send_clipboard
  Tempfile.create(['Clipboard Image ', '.png'], $dz.temp_folder) do |file|
    file.close
    stdout, stderr, status = Open3.capture3(
      '/usr/bin/osascript', '-e', CLIPBOARD_IMAGE_SCRIPT, file.path
    )

    unless status.success?
      detail = stderr.strip
      detail = stdout.strip if detail.empty?
      $dz.fail(detail.empty? ? 'Не удалось прочитать буфер обмена' : detail)
      return
    end

    if stdout.strip == 'NO_IMAGE'
      send_text($dz.read_clipboard)
      return
    end

    $dz.begin('Отправка изображения на RUGM0Q3RJ0...')
    error = run_cli([file.path])
    if error
      $dz.fail(error)
      return
    end

    $dz.finish('Изображение отправлено на RUGM0Q3RJ0')
    $dz.url(false)
  end
end

def dragged
  $dz.determinate(false)

  case ENV['dragged_type']
  when 'files'
    if $items.empty?
      $dz.fail('Нет файлов для отправки')
      return
    end

    $items.each_with_index do |path, index|
      $dz.begin("Отправка файла #{index + 1} из #{$items.length}...")
      error = run_cli([path])
      if error
        $dz.fail(error)
        return
      end
    end

    $dz.finish("Отправлено файлов: #{$items.length}")
    $dz.url(false)
  when 'text'
    send_text($items[0])
  else
    $dz.fail('Dropzone передал неподдерживаемый тип данных')
  end
end

def clicked
  $dz.determinate(false)
  send_clipboard
end
