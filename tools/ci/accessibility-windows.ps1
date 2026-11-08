Add-Type -AssemblyName System.Windows.Forms
$script:window = New-Object System.Windows.Forms.Form
$script:window.Text = 'DevWorks UIA Qualification'
$script:window.Width = 320
$script:window.Height = 180
$button = New-Object System.Windows.Forms.Button
$button.Text = 'Qualify Action'
$button.AccessibleName = 'Qualify Action'
$button.Width = 160
$button.Height = 40
$button.Left = 70
$button.Top = 50
$button.Add_Click({
    [System.IO.File]::WriteAllText($env:DEVWORKS_ACCESSIBILITY_MARKER, 'pressed')
    $timer = New-Object System.Windows.Forms.Timer
    $timer.Interval = 500
    $timer.Add_Tick({ $script:window.Close() })
    $timer.Start()
})
$script:window.Controls.Add($button)
$script:window.Add_Shown({ $script:window.Activate(); $button.Focus() })
[System.Windows.Forms.Application]::Run($script:window)
