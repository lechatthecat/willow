package main

import (
	"fmt"
	"os"
	"runtime"
	"strconv"
	"sync"
	"time"
)

type payload struct {
	value int64
}

func integerArg(index int) int {
	value, err := strconv.Atoi(os.Args[index])
	if err != nil || value <= 0 {
		panic("benchmark arguments must be positive integers")
	}
	return value
}

func idleSpawn(count int) {
	gate := make(chan struct{})
	time.Sleep(500 * time.Millisecond)
	fmt.Println("BASELINE")
	fmt.Println("SPAWN_START")
	for i := 0; i < count; i++ {
		go func() { <-gate }()
	}
	fmt.Println("SPAWN_DONE")
	time.Sleep(time.Second)
	var stats runtime.MemStats
	runtime.ReadMemStats(&stats)
	fmt.Println(stats.TotalAlloc)
	fmt.Println("READY")
	time.Sleep(time.Second)
}

func wakeFanout(count int) {
	wakes := make([]chan struct{}, 0, count)
	done := make(chan struct{}, count)
	for i := 0; i < count; i++ {
		wake := make(chan struct{}, 1)
		wakes = append(wakes, wake)
		go func(wake chan struct{}) {
			<-wake
			done <- struct{}{}
		}(wake)
	}
	time.Sleep(time.Second)
	fmt.Println("START")
	for i := 0; i < count; i++ {
		wakes[i] <- struct{}{}
	}
	for i := 0; i < count; i++ {
		<-done
	}
	fmt.Println("END")
}

func yieldSwitch(count, rounds int) {
	done := make(chan struct{}, count)
	fmt.Println("START")
	for i := 0; i < count; i++ {
		go func() {
			for j := 0; j < rounds; j++ {
				runtime.Gosched()
			}
			done <- struct{}{}
		}()
	}
	for i := 0; i < count; i++ {
		<-done
	}
	fmt.Println("END")
}

func pingPong(rounds int) {
	ping := make(chan struct{}, 1)
	pong := make(chan struct{}, 1)
	go func() {
		for i := 0; i < rounds; i++ {
			<-ping
			pong <- struct{}{}
		}
	}()
	fmt.Println("START")
	for i := 0; i < rounds; i++ {
		ping <- struct{}{}
		<-pong
	}
	fmt.Println("END")
}

// Four producers each feed their own capacity-1 channel; the consumer selects
// across all four until it has taken `count` tokens, then cancels the
// producers (close(done)) and joins them inside the timed phase, so the
// wake/cancel path is part of the measurement.
func channelSelectFanIn(count int) {
	const producers = 4
	channels := make([]chan struct{}, producers)
	done := make(chan struct{})
	var wg sync.WaitGroup
	for i := range channels {
		channels[i] = make(chan struct{}, 1)
		out := channels[i]
		wg.Add(1)
		go func() {
			defer wg.Done()
			for {
				select {
				case out <- struct{}{}:
				case <-done:
					return
				}
			}
		}()
	}
	fmt.Println("START")
	received := 0
	for received < count {
		select {
		case <-channels[0]:
		case <-channels[1]:
		case <-channels[2]:
		case <-channels[3]:
		}
		received++
	}
	close(done)
	wg.Wait()
	fmt.Println("END")
	fmt.Println("RECEIVED")
	fmt.Println(received)
}

// Every node spawns `fanout` children, waits for all of them (WaitGroup as the
// structured-concurrency join) and sums their leaf counts.
func treeNode(depth, fanout int) int {
	if depth == 0 {
		return 1
	}
	results := make([]int, fanout)
	var wg sync.WaitGroup
	wg.Add(fanout)
	for i := 0; i < fanout; i++ {
		go func(slot int) {
			defer wg.Done()
			results[slot] = treeNode(depth-1, fanout)
		}(i)
	}
	wg.Wait()
	sum := 0
	for _, value := range results {
		sum += value
	}
	return sum
}

func spawnJoinTree(fanout, depth int) {
	fmt.Println("START")
	leaves := treeNode(depth, fanout)
	fmt.Println("END")
	fmt.Println("LEAVES")
	fmt.Println(leaves)
}

func gcScheduler(count, rounds int) {
	var before runtime.MemStats
	runtime.ReadMemStats(&before)
	done := make(chan int, count)
	fmt.Println("START")
	for i := 0; i < count; i++ {
		go func() {
			values := make([]*payload, 0, rounds)
			for j := 0; j < rounds; j++ {
				values = append(values, &payload{value: int64(j)})
				runtime.Gosched()
			}
			runtime.KeepAlive(values)
			done <- len(values)
		}()
	}
	kept := 0
	for i := 0; i < count; i++ {
		kept += <-done
	}
	fmt.Println("END")
	var after runtime.MemStats
	runtime.ReadMemStats(&after)
	maxPause := uint64(0)
	for cycle := before.NumGC + 1; cycle <= after.NumGC; cycle++ {
		pause := after.PauseNs[(cycle+255)%256]
		if pause > maxPause {
			maxPause = pause
		}
	}
	fmt.Println("KEPT_OBJECTS")
	fmt.Println(kept)
	fmt.Println("GO_TOTAL_ALLOC_BYTES")
	fmt.Println(after.TotalAlloc - before.TotalAlloc)
	fmt.Println("GO_MALLOCS")
	fmt.Println(after.Mallocs - before.Mallocs)
	fmt.Println("GO_HEAP_ALLOC_BYTES")
	fmt.Println(after.HeapAlloc)
	fmt.Println("GO_GC_CYCLES")
	fmt.Println(after.NumGC - before.NumGC)
	fmt.Println("GO_GC_PAUSE_TOTAL_NS")
	fmt.Println(after.PauseTotalNs - before.PauseTotalNs)
	fmt.Println("GO_GC_MAX_PAUSE_NS")
	fmt.Println(maxPause)
}

func main() {
	if len(os.Args) < 3 {
		panic("usage: go-bench <idle_spawn|wake_fanout|yield_switch|ping_pong|gc_scheduler|channel_select_fan_in|spawn_join_tree> <count> [rounds]")
	}
	switch os.Args[1] {
	case "idle_spawn":
		idleSpawn(integerArg(2))
	case "wake_fanout":
		wakeFanout(integerArg(2))
	case "yield_switch":
		yieldSwitch(integerArg(2), integerArg(3))
	case "ping_pong":
		pingPong(integerArg(2))
	case "gc_scheduler":
		gcScheduler(integerArg(2), integerArg(3))
	case "channel_select_fan_in":
		channelSelectFanIn(integerArg(2))
	case "spawn_join_tree":
		spawnJoinTree(integerArg(2), integerArg(3))
	default:
		panic("unknown benchmark")
	}
}
