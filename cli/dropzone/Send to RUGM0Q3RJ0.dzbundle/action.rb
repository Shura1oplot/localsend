# Dropzone Action Info
# Name: Send to RUGM0Q3RJ0
# Description: Sends dropped files or text to RUGM0Q3RJ0 through LocalSend CLI. A click sends the clipboard text.
# Handles: Files, Text
# Creator: Alexander Gordeev
# URL: https://github.com/localsend/localsend
# Events: Clicked, Dragged
# SkipConfig: Yes
# RunsSandboxed: No
# Version: 1.0
# MinDropzoneVersion: 5.0
# UniqueID: 837421905

require 'open3'

CLI_PATH = File.expand_path('~/.local/bin/localsend-cli').freeze

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
  send_text($dz.read_clipboard)
end
