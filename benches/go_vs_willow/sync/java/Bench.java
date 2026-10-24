import java.lang.management.GarbageCollectorMXBean;
import java.lang.ref.Reference;
import java.lang.management.ManagementFactory;
import java.util.HashMap;
import java.util.function.LongUnaryOperator;

/**
 * Synchronous workloads equivalent to sync/go/main.go and sync/willow/*.wi.
 * Each case runs once on the main thread and prints a single validated result;
 * the *_only cases also print START/END phase markers.
 */
public final class Bench {
    private static final int LEIBNIZ_LIMIT = 100_000_000;
    private static final int ARRAY_COUNT = 5_000_000;
    private static final int LIST_COUNT = 1_000_000;
    private static final int CHURN_COUNT = 10_000_000;
    private static final int CHURN_RING = 1024;
    private static final int MAP_COUNT = 1_000_000;
    private static final long MAP_KEY_STRIDE = 2654435761L;
    private static final int SHAPE_COUNT = 1000;
    private static final int DISPATCH_ROUNDS = 20000;

    /** Unboxed growable array: the Java analogue of a Go []int64 / Willow Array<i64>. */
    private static final class LongList {
        private long[] data;
        private int size;

        LongList(int capacity) {
            data = new long[Math.max(capacity, 1)];
        }

        void add(long value) {
            if (size == data.length) {
                data = java.util.Arrays.copyOf(data, data.length * 2);
            }
            data[size++] = value;
        }

        long get(int index) {
            return data[index];
        }

        void set(int index, long value) {
            data[index] = value;
        }

        int size() {
            return size;
        }
    }

    private static final class Node {
        final long value;
        final Node next;

        Node(long value, Node next) {
            this.value = value;
            this.next = next;
        }
    }

    private static final class Payload {
        final long value;

        Payload(long value) {
            this.value = value;
        }
    }

    private interface Shape {
        long area();
    }

    private static final class Square implements Shape {
        final long side;

        Square(long side) {
            this.side = side;
        }

        public long area() {
            return side * side;
        }
    }

    private static final class Rect implements Shape {
        final long width;
        final long height;

        Rect(long width, long height) {
            this.width = width;
            this.height = height;
        }

        public long area() {
            return width * height;
        }
    }

    private static final class Triangle implements Shape {
        final long base;
        final long height;

        Triangle(long base, long height) {
            this.base = base;
            this.height = height;
        }

        public long area() {
            return base * height / 2;
        }
    }

    private static final class Circle implements Shape {
        final long radius;

        Circle(long radius) {
            this.radius = radius;
        }

        public long area() {
            return 3 * radius * radius;
        }
    }

    private static long gcCycles() {
        long total = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long count = gc.getCollectionCount();
            if (count > 0) {
                total += count;
            }
        }
        return total;
    }

    // Cumulative collector time in ms (1 ms resolution), not pause time.
    private static long gcTimeMs() {
        long total = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            long time = gc.getCollectionTime();
            if (time > 0) {
                total += time;
            }
        }
        return total;
    }

    private static long threadAllocatedBytes() {
        com.sun.management.ThreadMXBean threads =
            (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();
        return threads.getThreadAllocatedBytes(Thread.currentThread().threadId());
    }

    // Short-lived allocation churn: each new Payload evicts the one written 1,024
    // allocations earlier, so objects die young and the ring store defeats scalar
    // replacement. Metrics follow the result line as name/value pairs.
    private static void objectChurn() {
        Payload[] ring = new Payload[CHURN_RING];
        for (int i = 0; i < CHURN_RING; i++) {
            ring[i] = new Payload(0);
        }
        long allocBefore = threadAllocatedBytes();
        long cyclesBefore = gcCycles();
        long timeBefore = gcTimeMs();
        long sum = 0;
        marker("START");
        for (long i = 0; i < CHURN_COUNT; i++) {
            int slot = (int) (i % CHURN_RING);
            sum += ring[slot].value;
            ring[slot] = new Payload(i % 1000);
        }
        marker("END");
        long allocAfter = threadAllocatedBytes();
        long cyclesAfter = gcCycles();
        long timeAfter = gcTimeMs();
        System.out.println(sum);
        System.out.println("JAVA_TOTAL_ALLOC_BYTES");
        System.out.println(allocAfter - allocBefore);
        System.out.println("JAVA_HEAP_USED_BYTES");
        System.out.println(ManagementFactory.getMemoryMXBean().getHeapMemoryUsage().getUsed());
        System.out.println("JAVA_GC_CYCLES");
        System.out.println(cyclesAfter - cyclesBefore);
        System.out.println("JAVA_GC_TIME_TOTAL_NS");
        System.out.println((timeAfter - timeBefore) * 1_000_000L);
    }

    // objectChurn with a three-element array literal in place of the object.
    private static void arrayLiteralChurn() {
        long[][] ring = new long[CHURN_RING][];
        for (int i = 0; i < CHURN_RING; i++) {
            ring[i] = new long[] {0, 0, 0};
        }
        long allocBefore = threadAllocatedBytes();
        long cyclesBefore = gcCycles();
        long timeBefore = gcTimeMs();
        long sum = 0;
        marker("START");
        for (long i = 0; i < CHURN_COUNT; i++) {
            int slot = (int) (i % CHURN_RING);
            sum += ring[slot][0];
            ring[slot] = new long[] {i % 1000, slot, 1};
        }
        marker("END");
        long allocAfter = threadAllocatedBytes();
        long cyclesAfter = gcCycles();
        long timeAfter = gcTimeMs();
        System.out.println(sum);
        System.out.println("JAVA_TOTAL_ALLOC_BYTES");
        System.out.println(allocAfter - allocBefore);
        System.out.println("JAVA_GC_CYCLES");
        System.out.println(cyclesAfter - cyclesBefore);
        System.out.println("JAVA_GC_TIME_TOTAL_NS");
        System.out.println((timeAfter - timeBefore) * 1_000_000L);
    }

    // Boxed HashMap<Long, Long>: Java has no primitive-keyed map in the JDK, so
    // this includes Long boxing on every insert and lookup.
    private static long mapLookupInsert() {
        HashMap<Long, Long> table = new HashMap<>();
        marker("START");
        for (long i = 0; i < MAP_COUNT; i++) {
            table.put(i * MAP_KEY_STRIDE, i);
        }
        long sum = 0;
        for (long i = 0; i < 2L * MAP_COUNT; i++) {
            Long value = table.get(i * MAP_KEY_STRIDE);
            if (value != null) {
                sum += value;
            } else {
                sum += 1;
            }
        }
        marker("END");
        return sum;
    }

    // Megamorphic interface call: four receiver classes rotate through one site.
    private static long virtualDispatch() {
        Shape[] shapes = new Shape[SHAPE_COUNT];
        for (int i = 0; i < SHAPE_COUNT; i++) {
            long n = i % 10 + 1;
            switch (i % 4) {
                case 0 -> shapes[i] = new Square(n);
                case 1 -> shapes[i] = new Rect(n, n + 1);
                case 2 -> shapes[i] = new Triangle(n, n + 2);
                default -> shapes[i] = new Circle(n);
            }
        }
        long sum = 0;
        marker("START");
        for (int round = 0; round < DISPATCH_ROUNDS; round++) {
            for (int i = 0; i < shapes.length; i++) {
                sum += shapes[i].area();
            }
        }
        marker("END");
        return sum;
    }

    private static LongUnaryOperator adder(long n) {
        return x -> x + n;
    }

    // Capturing closures called through an array: the target is a run-time value.
    private static long closureCall() {
        LongUnaryOperator[] fs = new LongUnaryOperator[1024];
        for (int i = 0; i < fs.length; i++) {
            fs[i] = adder(i % 7 + 1);
        }
        long sum = 0;
        marker("START");
        for (int round = 0; round < 20_000; round++) {
            for (int i = 0; i < fs.length; i++) {
                sum += fs[i].applyAsLong(i);
            }
        }
        marker("END");
        return sum;
    }

    // Two integer-to-decimal conversions and three concatenations per string.
    private static long stringBuild() {
        long total = 0;
        marker("START");
        for (long i = 0; i < 2_000_000; i++) {
            String s = "k" + Long.toString(i) + ":" + Long.toString(i * 7);
            total += s.length();
        }
        marker("END");
        return total;
    }

    // Exact binary increments; no integral or square-root shortcut.
    private static double generalPow() {
        double x = 1.125, sum = 0.0;
        for (int i = 0; i < 20_480_000; i++) {
            sum += Math.pow(x, 0.375);
            x += 0.0009765625;
            if (x >= 2.125) {
                x = 1.125;
            }
        }
        return sum;
    }

    private static double leibnizPow() {
        double sum = 0.0;
        for (double n = 0.0; n <= (double) LEIBNIZ_LIMIT; n++) {
            sum += Math.pow(-1.0, n) / (2.0 * n + 1.0);
        }
        return 4.0 * sum;
    }

    // Non-unit base: pow cannot return +/-1 without multiplying.
    private static double leibnizPowNonunit() {
        double sum = 0.0;
        for (double n = 0.0; n <= (double) LEIBNIZ_LIMIT; n++) {
            sum += Math.pow(-1.0000001, n) / (2.0 * n + 1.0);
        }
        return 4.0 * sum;
    }

    private static double leibnizReduced() {
        double sum = 0.0;
        double sign = 1.0;
        double denominator = 1.0;
        for (int i = 0; i <= LEIBNIZ_LIMIT; i++) {
            sum += sign / denominator;
            sign = -sign;
            denominator += 2.0;
        }
        return 4.0 * sum;
    }

    private static long fib(long n) {
        if (n < 2) {
            return n;
        }
        return fib(n - 1) + fib(n - 2);
    }

    private static long arraySum() {
        LongList values = new LongList(1);
        for (long i = 0; i < ARRAY_COUNT; i++) {
            values.add(i % 1000);
        }
        long sum = 0;
        for (int i = 0; i < values.size(); i++) {
            sum += values.get(i);
        }
        return sum;
    }

    private static void marker(String name) {
        System.out.println(name);
        System.out.flush();
    }

    // Markers match the Willow/Go phase boundaries; validation stays outside them.
    private static long arrayPhase(String kind) {
        long result = 0;
        if (kind.equals("array_grow_only")) {
            marker("START");
            for (int round = 0; round < 1000; round++) {
                LongList batch = new LongList(1);
                for (long i = 0; i < 5000; i++) {
                    batch.add(i % 1000);
                }
                result += batch.size();
                Reference.reachabilityFence(batch);
            }
            marker("END");
            return result;
        }
        LongList values = new LongList(1);
        if (kind.equals("array_build_only")) {
            marker("START");
        }
        for (long i = 0; i < ARRAY_COUNT; i++) {
            values.add(i % 1000);
        }
        if (kind.equals("array_build_only")) {
            marker("END");
            Reference.reachabilityFence(values);
            return values.size();
        }
        marker("START");
        if (kind.equals("array_read_sum_only")) {
            for (int i = 0; i < values.size(); i++) {
                result += values.get(i);
            }
        } else {
            for (int i = 0; i < values.size(); i++) {
                values.set(i, (long) (i % 1000) + 1);
            }
        }
        marker("END");
        if (kind.equals("array_write_only")) {
            for (int i = 0; i < values.size(); i++) {
                result += values.get(i);
            }
        }
        return result;
    }

    private static long linkedList() {
        Node list = null;
        for (long i = 0; i < LIST_COUNT; i++) {
            list = new Node(i, list);
        }
        long sum = 0;
        for (Node current = list; current != null; current = current.next) {
            sum += current.value;
        }
        return sum;
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            throw new IllegalArgumentException(
                "usage: Bench <general_pow|leibniz_pow|leibniz_pow_nonunit|leibniz_reduced|fibonacci|array_sum|linked_list|array_*_only"
                    + "|object_churn|array_literal_churn|map_lookup_insert|virtual_dispatch|closure_call|string_build>");
        }
        switch (args[0]) {
            case "general_pow" -> System.out.println(generalPow());
            case "leibniz_pow" -> System.out.println(leibnizPow());
            case "leibniz_pow_nonunit" -> System.out.println(leibnizPowNonunit());
            case "leibniz_reduced" -> System.out.println(leibnizReduced());
            case "fibonacci" -> System.out.println(fib(40));
            case "array_sum" -> System.out.println(arraySum());
            case "array_build_only", "array_read_sum_only", "array_write_only", "array_grow_only" ->
                System.out.println(arrayPhase(args[0]));
            case "linked_list" -> System.out.println(linkedList());
            case "object_churn" -> objectChurn();
            case "array_literal_churn" -> arrayLiteralChurn();
            case "map_lookup_insert" -> System.out.println(mapLookupInsert());
            case "virtual_dispatch" -> System.out.println(virtualDispatch());
            case "closure_call" -> System.out.println(closureCall());
            case "string_build" -> System.out.println(stringBuild());
            default -> throw new IllegalArgumentException("unknown benchmark: " + args[0]);
        }
    }
}
