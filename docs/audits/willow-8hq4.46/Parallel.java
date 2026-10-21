public class Parallel {
    public static void main(String[] args) throws Exception {
        int workers = Integer.parseInt(System.getenv().getOrDefault("WILLOW_WORKERS", "8"));
        long[] input = new long[4_000_000];
        for (int i = 0; i < input.length; i++) input[i] = i;
        long[] output = null;
        Thread.sleep(20);
        System.out.println("@start work");
        for (int round = 0; round < 5; round++) {
            long[] result = new long[input.length];
            Thread[] threads = new Thread[workers];
            for (int w = 0; w < workers; w++) {
                int start = w * input.length / workers;
                int end = (w + 1) * input.length / workers;
                threads[w] = new Thread(() -> {
                    for (int i = start; i < end; i++) {
                        long x = input[i];
                        x ^= x << 13; x ^= x >> 7; x ^= x << 17;
                        result[i] = x & 65535;
                    }
                });
                threads[w].start();
            }
            for (Thread thread : threads) thread.join();
            output = result;
        }
        System.out.println("@end work");
        long sum = 0;
        for (long x : output) sum += x;
        System.out.println(sum);
    }
}
