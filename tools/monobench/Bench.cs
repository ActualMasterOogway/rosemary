using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Runtime.InteropServices;

interface IShape { int Area(); }
class Sq : IShape { public int s; public int Area() { return s * s; } }
class Rect : IShape { public int w, h; public int Area() { return w * h; } }
abstract class Animal { public abstract int Legs(); }
class Dog : Animal { public override int Legs() { return 4; } }
class Bird : Animal { public override int Legs() { return 2; } }

static class Gen<T> { public static int Touch(T v) { return v.GetHashCode() & 1; } }

static class Bench {
    [DllImport("libc")] static extern int getpid();

    static string only = Environment.GetEnvironmentVariable("BENCH_ONLY");
    static int scale = int.Parse(Environment.GetEnvironmentVariable("BENCH_SCALE") ?? "1");

    static void Phase(string name, Func<long> body) {
        if (only != null && only != name) return;
        var sw = Stopwatch.StartNew();
        long r = body();
        sw.Stop();
        Console.WriteLine("{0,-12} {1,9:F1} ms  ({2})", name, sw.Elapsed.TotalMilliseconds, r);
    }

    static int Fib(int n) { return n < 2 ? n : Fib(n - 1) + Fib(n - 2); }

    static void Main() {
        // First calls: many generic instantiations, each JIT-compiled once.
        Phase("jit-churn", () => {
            long t = 0;
            t += Gen<int>.Touch(1); t += Gen<long>.Touch(1); t += Gen<short>.Touch(1);
            t += Gen<byte>.Touch(1); t += Gen<sbyte>.Touch(1); t += Gen<ushort>.Touch(1);
            t += Gen<uint>.Touch(1); t += Gen<ulong>.Touch(1); t += Gen<float>.Touch(1);
            t += Gen<double>.Touch(1); t += Gen<decimal>.Touch(1); t += Gen<char>.Touch('a');
            t += Gen<bool>.Touch(true); t += Gen<DateTime>.Touch(DateTime.MinValue);
            t += Gen<TimeSpan>.Touch(TimeSpan.Zero); t += Gen<Guid>.Touch(Guid.Empty);
            var l = new List<string>(); l.Add("x"); l.Sort(); t += l.Count;
            var d = new Dictionary<string, int>(); d["a"] = 1; t += d["a"];
            t += string.Format("{0}{1}", 1, 2.5).Length;
            t += System.Linq.Enumerable.Sum(System.Linq.Enumerable.Range(0, 10));
            return t;
        });
        // Steady state: one hot method, compiled once.
        Phase("loop", () => {
            long s = 0;
            for (int i = 0; i < 200000000 * scale; i++) s += i ^ (s >> 3);
            return s;
        });
        Phase("recursion", () => Fib(30 + scale));
        Phase("virtual", () => {
            Animal[] a = { new Dog(), new Bird() };
            long s = 0;
            for (int i = 0; i < 20000000 * scale; i++) s += a[i & 1].Legs();
            return s;
        });
        Phase("interface", () => {
            IShape[] a = { new Sq { s = 3 }, new Rect { w = 2, h = 5 } };
            long s = 0;
            for (int i = 0; i < 20000000 * scale; i++) s += a[i & 1].Area();
            return s;
        });
        Phase("delegate", () => {
            Func<int, int> f = x => x + 1;
            long s = 0;
            for (int i = 0; i < 20000000 * scale; i++) s += f(i);
            return s;
        });
        Phase("pinvoke", () => {
            long s = 0;
            for (int i = 0; i < 1000000 * scale; i++) s += getpid() & 1;
            return s;
        });
        Phase("alloc", () => {
            long s = 0;
            for (int i = 0; i < 5000000 * scale; i++) { var o = new int[4]; o[0] = i; s += o[0]; }
            return s;
        });
        Phase("strings", () => {
            long s = 0;
            for (int i = 0; i < 300000 * scale; i++) s += (i.ToString() + "x").Length;
            return s;
        });
    }
}
