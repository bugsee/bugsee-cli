using System;
using System.Collections.Generic;
using System.Linq;
using System.Threading.Tasks;

namespace Demo.App
{
    public delegate void Callback(int code);

    public class Plain
    {
        public static int Counter;
        static Plain() { Counter = 1; }
        public Plain(string name) { Name = name; }
        public string Name { get; set; }
        public int Add(int a, int b)
        {
            var r = a + b;
            return r;
        }
        public static unsafe int Peek(int* p, byte[] bytes, int[,] grid)
        {
            return *p + bytes.Length + grid.Length;
        }
        public virtual void Take(in int value, ref string text, out double d)
        {
            d = value;
            text += "x";
        }
        public static Plain operator +(Plain l, Plain r) { return new Plain(l.Name + r.Name); }
    }

    public class Box<T>
    {
        public T Value;
        public class Inner
        {
            public int Depth(T item, List<T> all, Dictionary<string, List<int>> map)
            {
                return all.Count + map.Count + (item == null ? 0 : 1);
            }
        }
        public U Convert<U, V>(Func<T, U> f, V extra, T[] many)
        {
            Console.WriteLine(extra);
            return f(many[0]);
        }
    }

    public static class Work
    {
        public static int Lambdas(int seed)
        {
            int captured = seed * 2;
            Func<int, int> closure = n => n + captured;
            Func<int, int> stateless = n => n * 3;
            return closure(1) + stateless(2);
        }

        public static async Task<int> Fetch(string url, int delayMs)
        {
            await Task.Delay(delayMs);
            if (url == null) throw new ArgumentNullException(nameof(url));
            return url.Length;
        }

        public static async Task Gen<T>(T value, List<T> sink)
        {
            await Task.Yield();
            sink.Add(value);
        }

        public static IEnumerable<int> Iterate(int upTo)
        {
            for (int i = 0; i < upTo; i++) yield return i;
        }

        public static int Anonymous()
        {
            var items = new[] { new { Id = 1, Label = "a" }, new { Id = 2, Label = "b" } };
            return items.Select(x => x.Id).Sum() + items.Count(x => x.Label == "b");
        }

        public static void Local(int n)
        {
            int Twice(int v) => v * 2;
            Console.WriteLine(Twice(n));
        }
    }
}
