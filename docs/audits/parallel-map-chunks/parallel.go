package main

import (
 "fmt"
 "os"
 "strconv"
 "sync"
 "time"
)

func main() {
 workers, _ := strconv.Atoi(os.Getenv("WILLOW_WORKERS")); if workers < 1 { workers = 8 }
 input := make([]int64, 4000000)
 for i := range input { input[i] = int64(i) }
 var output []int64
 time.Sleep(20*time.Millisecond)
 fmt.Println("@start work")
 for round := 0; round < 5; round++ {
  output = make([]int64, len(input))
  var wg sync.WaitGroup
  for w := 0; w < workers; w++ {
   start, end := w*len(input)/workers, (w+1)*len(input)/workers
   wg.Add(1)
   go func() {
    defer wg.Done()
    for i := start; i < end; i++ {
     x := input[i]; x ^= x << 13; x ^= x >> 7; x ^= x << 17
     output[i] = x & 65535
    }
   }()
  }
  wg.Wait()
 }
 fmt.Println("@end work")
 var sum int64
 for _, x := range output { sum += x }
 fmt.Println(sum)
}
