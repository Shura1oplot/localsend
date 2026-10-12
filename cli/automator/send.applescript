-- Automator "Run AppleScript" action for "Send to RUGM0Q3RJ0.app".
-- Files dropped on the app are sent one by one.
-- A click sends the clipboard image or, if there is none, the clipboard text.

on run {input, parameters}
	set cliPath to POSIX path of (path to home folder) & ".local/bin/localsend-cli"
	try
		do shell script "test -x " & quoted form of cliPath
	on error
		fail("Программа не найдена или не исполняемая: " & cliPath)
		return input
	end try

	with timeout of 7200 seconds
		if (count of input) is 0 then
			sendClipboard(cliPath)
		else
			sendFiles(input, cliPath)
		end if
	end timeout
	return input
end run

on sendFiles(fileList, cliPath)
	set total to count of fileList
	repeat with i from 1 to total
		set filePath to POSIX path of (item i of fileList)
		if not runCli(cliPath, quoted form of filePath) then return
	end repeat
	notify("Отправлено файлов: " & total)
end sendFiles

on sendClipboard(cliPath)
	try
		set imageData to the clipboard as «class PNGf»
	on error
		sendText(cliPath)
		return
	end try

	set tempFolder to do shell script "/usr/bin/mktemp -d -t localsend"
	set imagePath to tempFolder & "/Clipboard Image.png"
	try
		set fileHandle to open for access (POSIX file imagePath) with write permission
		try
			set eof fileHandle to 0
			write imageData to fileHandle
			close access fileHandle
		on error errorMessage number errorNumber
			close access fileHandle
			error errorMessage number errorNumber
		end try
	on error errorMessage
		do shell script "rm -rf " & quoted form of tempFolder
		fail("Не удалось сохранить изображение из буфера обмена: " & errorMessage)
		return
	end try

	set succeeded to runCli(cliPath, quoted form of imagePath)
	do shell script "rm -rf " & quoted form of tempFolder
	if succeeded then notify("Изображение отправлено на RUGM0Q3RJ0")
end sendClipboard

on sendText(cliPath)
	try
		set clipboardText to the clipboard as text
	on error
		set clipboardText to ""
	end try
	if clipboardText is "" then
		fail("В буфере обмена нет изображения или текста")
		return
	end if

	if runCli(cliPath, "--text " & quoted form of clipboardText) then
		notify("Текст отправлен на RUGM0Q3RJ0")
	end if
end sendText

on runCli(cliPath, arguments)
	try
		do shell script quoted form of cliPath & " " & arguments
		return true
	on error errorMessage number errorNumber
		if errorMessage is "" then set errorMessage to "localsend-cli завершился с кодом " & errorNumber
		fail(errorMessage)
		return false
	end try
end runCli

on notify(notificationText)
	display notification notificationText with title "LocalSend"
end notify

on fail(alertText)
	activate
	display alert "Не удалось отправить на RUGM0Q3RJ0" message alertText as critical
end fail
