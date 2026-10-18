// Newly reconstructed for willow-8hq4.42; not recovered historical source.
package main

import (
	"fmt"
	"os"
	"runtime"
	"sync"
)

func main() {
	runtime.GOMAXPROCS(8)
	tasks, iterations := 8, 200000
	if len(os.Args) > 1 && os.Args[1] == "mutex_split" {
		tasks, iterations = 1, 1600000
	}
	var mu sync.Mutex
	var value int64
	var done sync.WaitGroup
	done.Add(tasks)
	fmt.Println("@start work")
	for t := 0; t < tasks; t++ {
		go func() {
			defer done.Done()
			for i := 0; i < iterations; i++ {
				mu.Lock()
				value = value + 1
				mu.Unlock()
			}
		}()
	}
	done.Wait()
	fmt.Println("@end work")
	mu.Lock()
	fmt.Println(value)
	mu.Unlock()
}
