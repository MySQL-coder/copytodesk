using System;
using System.Runtime.InteropServices;

namespace CopyToDesk.Core
{
    public interface IInputInjector
    {
        string BackendName { get; }
        long TotalEvents { get; }
        void InjectMouse(MouseEventPacket ev);
        void InjectKey(KeyEventPacket ev);
    }

    public class DryRunAuditInjector : IInputInjector
    {
        private long _count;
        public string BackendName { get { return "DryRun-Audit"; } }
        public long TotalEvents { get { return _count; } }

        public void InjectMouse(MouseEventPacket ev)
        {
            _count++;
        }

        public void InjectKey(KeyEventPacket ev)
        {
            _count++;
        }
    }

    public class Win32SendInputInjector : IInputInjector
    {
        private long _count;
        public string BackendName { get { return "Win32-SendInput"; } }
        public long TotalEvents { get { return _count; } }

        private const uint INPUT_MOUSE = 0;
        private const uint INPUT_KEYBOARD = 1;

        private const uint MOUSEEVENTF_MOVE = 0x0001;
        private const uint MOUSEEVENTF_LEFTDOWN = 0x0002;
        private const uint MOUSEEVENTF_LEFTUP = 0x0004;
        private const uint MOUSEEVENTF_RIGHTDOWN = 0x0008;
        private const uint MOUSEEVENTF_RIGHTUP = 0x0010;
        private const uint MOUSEEVENTF_MIDDLEDOWN = 0x0020;
        private const uint MOUSEEVENTF_MIDDLEUP = 0x0040;
        private const uint MOUSEEVENTF_WHEEL = 0x0800;
        private const uint MOUSEEVENTF_VIRTUALDESK = 0x4000;
        private const uint MOUSEEVENTF_ABSOLUTE = 0x8000;

        private const uint KEYEVENTF_EXTENDEDKEY = 0x0001;
        private const uint KEYEVENTF_KEYUP = 0x0002;
        private const uint KEYEVENTF_UNICODE = 0x0004;

        [StructLayout(LayoutKind.Sequential)]
        private struct INPUT
        {
            public uint type;
            public InputUnion U;
        }

        [StructLayout(LayoutKind.Explicit)]
        private struct InputUnion
        {
            [FieldOffset(0)] public MOUSEINPUT mi;
            [FieldOffset(0)] public KEYBDINPUT ki;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct MOUSEINPUT
        {
            public int dx;
            public int dy;
            public uint mouseData;
            public uint dwFlags;
            public uint time;
            public IntPtr dwExtraInfo;
        }

        [StructLayout(LayoutKind.Sequential)]
        private struct KEYBDINPUT
        {
            public ushort wVk;
            public ushort wScan;
            public uint dwFlags;
            public uint time;
            public IntPtr dwExtraInfo;
        }

        [DllImport("user32.dll", SetLastError = true)]
        private static extern uint SendInput(uint nInputs, INPUT[] pInputs, int cbSize);

        public void InjectMouse(MouseEventPacket ev)
        {
            _count++;
            INPUT[] inp = new INPUT[1];
            inp[0].type = INPUT_MOUSE;
            inp[0].U.mi.dx = ev.NormX;
            inp[0].U.mi.dy = ev.NormY;
            uint flags = MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK;

            if (ev.Action == MouseAction.Move)
            {
                flags |= MOUSEEVENTF_MOVE;
            }
            else if (ev.Action == MouseAction.Down)
            {
                flags |= MOUSEEVENTF_MOVE;
                if (ev.Button == MouseButton.Left) flags |= MOUSEEVENTF_LEFTDOWN;
                else if (ev.Button == MouseButton.Right) flags |= MOUSEEVENTF_RIGHTDOWN;
                else if (ev.Button == MouseButton.Middle) flags |= MOUSEEVENTF_MIDDLEDOWN;
            }
            else if (ev.Action == MouseAction.Up)
            {
                flags |= MOUSEEVENTF_MOVE;
                if (ev.Button == MouseButton.Left) flags |= MOUSEEVENTF_LEFTUP;
                else if (ev.Button == MouseButton.Right) flags |= MOUSEEVENTF_RIGHTUP;
                else if (ev.Button == MouseButton.Middle) flags |= MOUSEEVENTF_MIDDLEUP;
            }
            else if (ev.Action == MouseAction.Wheel)
            {
                flags |= MOUSEEVENTF_WHEEL;
                inp[0].U.mi.mouseData = (uint)(int)ev.WheelDelta;
            }

            inp[0].U.mi.dwFlags = flags;
            SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
        }

        public void InjectKey(KeyEventPacket ev)
        {
            _count++;
            if (ev.Action == KeyAction.UnicodeChar)
            {
                INPUT[] pair = new INPUT[2];
                pair[0].type = INPUT_KEYBOARD;
                pair[0].U.ki.wVk = 0;
                pair[0].U.ki.wScan = (ushort)ev.UnicodeChar;
                pair[0].U.ki.dwFlags = KEYEVENTF_UNICODE;

                pair[1].type = INPUT_KEYBOARD;
                pair[1].U.ki.wVk = 0;
                pair[1].U.ki.wScan = (ushort)ev.UnicodeChar;
                pair[1].U.ki.dwFlags = KEYEVENTF_UNICODE | KEYEVENTF_KEYUP;
                SendInput(2, pair, Marshal.SizeOf(typeof(INPUT)));
                return;
            }

            INPUT[] inp = new INPUT[1];
            inp[0].type = INPUT_KEYBOARD;
            inp[0].U.ki.wVk = ev.Vk;
            inp[0].U.ki.wScan = ev.ScanCode;
            uint flags = 0;
            if (ev.Action == KeyAction.Up) flags |= KEYEVENTF_KEYUP;
            if ((ev.Flags & 1) != 0) flags |= KEYEVENTF_EXTENDEDKEY;
            inp[0].U.ki.dwFlags = flags;
            SendInput(1, inp, Marshal.SizeOf(typeof(INPUT)));
        }
    }
}
