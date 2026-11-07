import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.MemoryMXBean;
import java.lang.ref.Reference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ArrayBlockingQueue;
import java.util.concurrent.BlockingQueue;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Scheduler microbenchmarks equivalent to go/main.go and willow/*.wi, using
 * virtual threads as the goroutine/task analogue and bounded BlockingQueues as
 * the channel analogue. Every benchmark body, including the coordinating side,
 * runs on virtual threads so the timed actors match the Go/Willow setups.
 */
public final class Bench {
    private static final Object TOKEN = new Object();

    private static final class Payload {
        final long value;

        Payload(long value) {
            this.value = value;
        }
    }

    private static void marker(String name) {
        System.out.println(name);
        System.out.flush();
    }

    private static void metric(String name, long value) {
        System.out.println(name);
        System.out.println(value);
    }

    private static int integerArg(String[] args, int index) {
        int value;
        try {
            value = Integer.parseInt(args[index]);
        } catch (RuntimeException error) {
            throw new IllegalArgumentException("benchmark arguments must be positive integers", error);
        }
        if (value <= 0) {
            throw new IllegalArgumentException("benchmark arguments must be positive integers");
        }
        return value;
    }

    /** Cumulative bytes allocated by all threads, or -1 when the JVM does not expose it. */
    private static long totalAllocatedBytes() {
        var bean = ManagementFactory.getThreadMXBean();
        if (bean instanceof com.sun.management.ThreadMXBean sun && sun.isThreadAllocatedMemorySupported()) {
            return sun.getTotalThreadAllocatedBytes();
        }
        return -1;
    }

    private static long gcCount() {
        long count = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            count += Math.max(gc.getCollectionCount(), 0);
        }
        return count;
    }

    private static long gcTimeMillis() {
        long millis = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            millis += Math.max(gc.getCollectionTime(), 0);
        }
        return millis;
    }

    private static void sleep(long millis) throws InterruptedException {
        Thread.sleep(millis);
    }

    private static void idleSpawn(int count) throws InterruptedException {
        CountDownLatch gate = new CountDownLatch(1);
        Runnable worker = () -> {
            try {
                gate.await();
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        };
        sleep(500);
        marker("BASELINE");
        marker("SPAWN_START");
        for (int i = 0; i < count; i++) {
            Thread.startVirtualThread(worker);
        }
        marker("SPAWN_DONE");
        sleep(1000);
        System.out.println(totalAllocatedBytes());
        marker("READY");
        sleep(1000);
    }

    private static void wakeFanout(int count) throws InterruptedException {
        List<BlockingQueue<Object>> wakes = new ArrayList<>(count);
        BlockingQueue<Object> done = new ArrayBlockingQueue<>(count);
        for (int i = 0; i < count; i++) {
            BlockingQueue<Object> wake = new ArrayBlockingQueue<>(1);
            wakes.add(wake);
            Thread.startVirtualThread(() -> {
                try {
                    wake.take();
                    done.put(TOKEN);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
        }
        sleep(1000);
        marker("START");
        for (int i = 0; i < count; i++) {
            wakes.get(i).put(TOKEN);
        }
        for (int i = 0; i < count; i++) {
            done.take();
        }
        marker("END");
    }

    private static void yieldSwitch(int count, int rounds) throws InterruptedException {
        BlockingQueue<Object> done = new ArrayBlockingQueue<>(count);
        marker("START");
        for (int i = 0; i < count; i++) {
            Thread.startVirtualThread(() -> {
                for (int j = 0; j < rounds; j++) {
                    Thread.yield();
                }
                try {
                    done.put(TOKEN);
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
        }
        for (int i = 0; i < count; i++) {
            done.take();
        }
        marker("END");
    }

    private static void pingPong(int rounds) throws InterruptedException {
        BlockingQueue<Object> ping = new ArrayBlockingQueue<>(1);
        BlockingQueue<Object> pong = new ArrayBlockingQueue<>(1);
        Thread.startVirtualThread(() -> {
            try {
                for (int i = 0; i < rounds; i++) {
                    ping.take();
                    pong.put(TOKEN);
                }
            } catch (InterruptedException ignored) {
                Thread.currentThread().interrupt();
            }
        });
        marker("START");
        for (int i = 0; i < rounds; i++) {
            ping.put(TOKEN);
            pong.take();
        }
        marker("END");
    }

    // Java has no select over several queues, so the four producers share one
    // queue of capacity 4 (one slot per producer, matching four capacity-1
    // channels in total). Producers are cancelled with interrupt() and joined
    // inside the timed phase, as in the Go/Willow versions.
    private static void channelSelectFanIn(int count) throws InterruptedException {
        final int producers = 4;
        BlockingQueue<Object> queue = new ArrayBlockingQueue<>(producers);
        Thread[] threads = new Thread[producers];
        for (int i = 0; i < producers; i++) {
            threads[i] = Thread.startVirtualThread(() -> {
                try {
                    while (true) {
                        queue.put(TOKEN);
                    }
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
        }
        marker("START");
        int received = 0;
        while (received < count) {
            queue.take();
            received++;
        }
        for (Thread thread : threads) {
            thread.interrupt();
        }
        for (Thread thread : threads) {
            thread.join();
        }
        marker("END");
        metric("RECEIVED", received);
    }

    // One virtual-thread-per-task executor per node is the structured join:
    // try-with-resources closes it only after every child has finished.
    private static long treeNode(int depth, int fanout) throws Exception {
        if (depth == 0) {
            return 1;
        }
        List<Future<Long>> children = new ArrayList<>(fanout);
        try (ExecutorService scope = Executors.newVirtualThreadPerTaskExecutor()) {
            for (int i = 0; i < fanout; i++) {
                children.add(scope.submit(() -> treeNode(depth - 1, fanout)));
            }
        }
        long sum = 0;
        for (Future<Long> child : children) {
            sum += child.get();
        }
        return sum;
    }

    private static void spawnJoinTree(int fanout, int depth) throws Exception {
        marker("START");
        long leaves = treeNode(depth, fanout);
        marker("END");
        metric("LEAVES", leaves);
    }

    private static void gcScheduler(int count, int rounds) throws InterruptedException {
        long allocatedBefore = totalAllocatedBytes();
        long gcCountBefore = gcCount();
        long gcTimeBefore = gcTimeMillis();
        BlockingQueue<Integer> done = new ArrayBlockingQueue<>(count);
        marker("START");
        for (int i = 0; i < count; i++) {
            Thread.startVirtualThread(() -> {
                List<Payload> values = new ArrayList<>(rounds);
                for (int j = 0; j < rounds; j++) {
                    values.add(new Payload(j));
                    Thread.yield();
                }
                Reference.reachabilityFence(values);
                try {
                    done.put(values.size());
                } catch (InterruptedException ignored) {
                    Thread.currentThread().interrupt();
                }
            });
        }
        long kept = 0;
        for (int i = 0; i < count; i++) {
            kept += done.take();
        }
        marker("END");
        long allocatedAfter = totalAllocatedBytes();
        metric("KEPT_OBJECTS", kept);
        gcMetrics(allocatedBefore, allocatedAfter, gcCountBefore, gcTimeBefore);
    }

    private static void gcMetrics(long allocatedBefore, long allocatedAfter, long gcCountBefore, long gcTimeBefore) {
        MemoryMXBean memory = ManagementFactory.getMemoryMXBean();
        metric("JAVA_TOTAL_ALLOC_BYTES", allocatedBefore < 0 ? -1 : allocatedAfter - allocatedBefore);
        metric("JAVA_HEAP_USED_BYTES", memory.getHeapMemoryUsage().getUsed());
        metric("JAVA_GC_CYCLES", gcCount() - gcCountBefore);
        // GarbageCollectorMXBean exposes cumulative collection time at 1 ms resolution only.
        metric("JAVA_GC_TIME_TOTAL_NS", (gcTimeMillis() - gcTimeBefore) * 1_000_000L);
    }

    private record ServerRequest(long conn, long seq) {}

    private record ServerResponse(long[] body) {}

    /** The growable unboxed list a Go slice or Willow Array&lt;i64&gt; provides. */
    private static final class Latencies {
        long[] values = new long[0];
        int size;

        void add(long value) {
            if (size == values.length) {
                values = java.util.Arrays.copyOf(values, Math.max(4, size * 2));
            }
            values[size++] = value;
        }
    }

    private static final class ServerStats {
        final ReentrantLock lock = new ReentrantLock();
        long served;
    }

    /**
     * One simulated connection: a client and a handler virtual thread exchanging
     * a request and a response per round over a pair of capacity-1 queues, with a
     * short-lived object graph per request and one shared lock update.
     */
    private static void serverHandler(
            BlockingQueue<ServerRequest> requests,
            BlockingQueue<ServerResponse> responses,
            int rounds,
            ServerStats stats) throws InterruptedException {
        for (int i = 0; i < rounds; i++) {
            ServerRequest request = requests.take();
            long seq = request.seq();
            long conn = request.conn();
            long[] body = {seq, conn, seq + 1, conn + 1, seq + 2, conn + 2, seq + 3, conn + 3};
            stats.lock.lock();
            try {
                stats.served++;
            } finally {
                stats.lock.unlock();
            }
            responses.put(new ServerResponse(body));
        }
    }

    private static long serverClient(
            long conn,
            BlockingQueue<ServerRequest> requests,
            BlockingQueue<ServerResponse> responses,
            int rounds,
            Latencies latencies) throws InterruptedException {
        long checksum = 0;
        for (int seq = 0; seq < rounds; seq++) {
            long sent = System.nanoTime();
            requests.put(new ServerRequest(conn, seq));
            ServerResponse response = responses.take();
            latencies.add(System.nanoTime() - sent);
            checksum += response.body()[0] + response.body()[7];
        }
        return checksum;
    }

    /** Log-linear bucket: 64 sub-buckets per power of two, under 1.6% relative error. */
    private static int bucketIndex(long value) {
        int shift = 0;
        while (value >= 128) {
            value /= 2;
            shift++;
        }
        return shift * 128 + (int) value;
    }

    private static long bucketUpper(int index) {
        long upper = index % 128 + 1;
        for (int shift = index / 128; shift > 0; shift--) {
            upper *= 2;
        }
        return upper - 1;
    }

    private static long percentile(long[] histogram, long total, long percent) {
        long rank = (total * percent + 99) / 100;
        long seen = 0;
        for (int i = 0; i < histogram.length; i++) {
            seen += histogram[i];
            if (seen >= rank) {
                return bucketUpper(i);
            }
        }
        return -1;
    }

    private static void serverSim(int connections, int rounds) throws InterruptedException {
        ServerStats stats = new ServerStats();
        long[] checksums = new long[connections];
        Latencies[] latencies = new Latencies[connections];
        long allocatedBefore = totalAllocatedBytes();
        long gcCountBefore = gcCount();
        long gcTimeBefore = gcTimeMillis();

        marker("START");
        Thread[] threads = new Thread[2 * connections];
        for (int i = 0; i < connections; i++) {
            int slot = i;
            BlockingQueue<ServerRequest> requests = new ArrayBlockingQueue<>(1);
            BlockingQueue<ServerResponse> responses = new ArrayBlockingQueue<>(1);
            latencies[slot] = new Latencies();
            threads[2 * slot] = Thread.startVirtualThread(() -> run(
                () -> serverHandler(requests, responses, rounds, stats)));
            threads[2 * slot + 1] = Thread.startVirtualThread(() -> run(
                () -> checksums[slot] = serverClient(slot, requests, responses, rounds, latencies[slot])));
        }
        for (Thread thread : threads) {
            thread.join();
        }
        marker("END");
        long allocatedAfter = totalAllocatedBytes();

        long[] histogram = new long[128 * 48];
        long checksum = 0;
        long longest = 0;
        long samples = 0;
        for (int i = 0; i < connections; i++) {
            checksum += checksums[i];
            for (int j = 0; j < latencies[i].size; j++) {
                long latency = latencies[i].values[j];
                histogram[bucketIndex(latency)]++;
                longest = Math.max(longest, latency);
                samples++;
            }
        }
        metric("REQUESTS", stats.served);
        metric("CHECKSUM", checksum);
        metric("LATENCY_P50_NS", percentile(histogram, samples, 50));
        metric("LATENCY_P99_NS", percentile(histogram, samples, 99));
        metric("LATENCY_MAX_NS", longest);
        gcMetrics(allocatedBefore, allocatedAfter, gcCountBefore, gcTimeBefore);
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            throw new IllegalArgumentException(
                "usage: Bench <idle_spawn|wake_fanout|yield_switch|ping_pong|gc_scheduler"
                    + "|channel_select_fan_in|spawn_join_tree|server_sim> <count> [rounds]");
        }
        int count = integerArg(args, 1);
        Runnable body = switch (args[0]) {
            case "idle_spawn" -> () -> run(() -> idleSpawn(count));
            case "wake_fanout" -> () -> run(() -> wakeFanout(count));
            case "yield_switch" -> {
                int rounds = integerArg(args, 2);
                yield () -> run(() -> yieldSwitch(count, rounds));
            }
            case "ping_pong" -> () -> run(() -> pingPong(count));
            case "gc_scheduler" -> {
                int rounds = integerArg(args, 2);
                yield () -> run(() -> gcScheduler(count, rounds));
            }
            case "channel_select_fan_in" -> () -> run(() -> channelSelectFanIn(count));
            case "spawn_join_tree" -> {
                int depth = integerArg(args, 2);
                yield () -> run(() -> spawnJoinTree(count, depth));
            }
            case "server_sim" -> {
                int rounds = integerArg(args, 2);
                yield () -> run(() -> serverSim(count, rounds));
            }
            default -> throw new IllegalArgumentException("unknown benchmark: " + args[0]);
        };
        Thread driver = Thread.ofVirtual().name("bench-main").start(body);
        driver.join();
        if (failure != null) {
            throw failure;
        }
    }

    private static volatile Exception failure;

    @FunctionalInterface
    private interface Body {
        void run() throws Exception;
    }

    private static void run(Body body) {
        try {
            body.run();
        } catch (Exception error) {
            failure = error;
        }
    }
}
