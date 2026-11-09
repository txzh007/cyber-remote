# 调试用:打印主机光标位置、前台窗口,以及记事本编辑框里的文字
Add-Type @'
using System;
using System.Runtime.InteropServices;
using System.Text;
public static class Probe {
  [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
  [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
  [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowW(string cls, string title);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr FindWindowExW(IntPtr parent, IntPtr after, string cls, string title);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, StringBuilder l);
  [DllImport("user32.dll")] public static extern IntPtr SendMessageW(IntPtr h, uint msg, IntPtr w, IntPtr l);
  [DllImport("user32.dll", CharSet=CharSet.Unicode)] public static extern int GetWindowTextW(IntPtr h, StringBuilder s, int n);
}
'@
$p = New-Object Probe+POINT
[void][Probe]::GetCursorPos([ref]$p)
$fg = [Probe]::GetForegroundWindow()
$top = [Probe]::FindWindowW('Notepad', [NullString]::Value)
$edit = [Probe]::FindWindowExW($top, [IntPtr]::Zero, 'Edit', [NullString]::Value)
$len = [int][Probe]::SendMessageW($edit, 0x000E, [IntPtr]::Zero, [IntPtr]::Zero)
$sb = New-Object System.Text.StringBuilder ($len + 1)
[void][Probe]::SendMessageW($edit, 0x000D, [IntPtr]($len + 1), $sb)
$t = New-Object System.Text.StringBuilder 256
[void][Probe]::GetWindowTextW($top, $t, 256)
"cursor=$($p.X),$($p.Y) foreground=$fg notepad=$top title=$($t.ToString())"
$sel = [int64][Probe]::SendMessageW($edit, 0x00B0, [IntPtr]::Zero, [IntPtr]::Zero)  # EM_GETSEL
$first = [int][Probe]::SendMessageW($edit, 0x00CE, [IntPtr]::Zero, [IntPtr]::Zero)  # EM_GETFIRSTVISIBLELINE
"sel=$($sel -band 0xFFFF)..$(($sel -shr 16) -band 0xFFFF) len=$len firstLine=$first"
"text=[" +$sb.ToString().Replace("`r", '\r').Replace("`n", '\n') + "]"

