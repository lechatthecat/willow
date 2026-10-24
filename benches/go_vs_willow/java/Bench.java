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
        MemoryMXBean memory = ManagementFactory.getMemoryMXBean();
        metric("KEPT_OBJECTS", kept);
        metric("JAVA_TOTAL_ALLOC_BYTES", allocatedBefore < 0 ? -1 : allocatedAfter - allocatedBefore);
        metric("JAVA_HEAP_USED_BYTES", memory.getHeapMemoryUsage().getUsed());
        metric("JAVA_GC_CYCLES", gcCount() - gcCountBefore);
        // GarbageCollectorMXBean exposes cumulative collection time at 1 ms resolution only.
        metric("JAVA_GC_TIME_TOTAL_NS", (gcTimeMillis() - gcTimeBefore) * 1_000_000L);
    }

    public static void main(String[] args) throws Exception {
        if (args.length < 2) {
            throw new IllegalArgumentException(
                "usage: Bench <idle_spawn|wake_fanout|yield_switch|ping_pong|gc_scheduler"
                    + "|channel_select_fan_in|spawn_join_tree> <count> [rounds]");
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
