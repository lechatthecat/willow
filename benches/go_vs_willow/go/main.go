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
	fmt.Println("KEPT_OBJECTS")
	fmt.Println(kept)
	printGcMetrics(&before, &after)
}

// MemStats keeps the last 256 pauses, so the maximum covers at most those.
func printGcMetrics(before, after *runtime.MemStats) {
	maxPause := uint64(0)
	for cycle := before.NumGC + 1; cycle <= after.NumGC; cycle++ {
		pause := after.PauseNs[(cycle+255)%256]
		if pause > maxPause {
			maxPause = pause
		}
	}
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

type serverRequest struct {
	conn int64
	seq  int64
}

type serverResponse struct {
	body []int64
}

// One simulated connection: a client goroutine and a handler goroutine
// exchanging a request and a response per round over a pair of capacity-1
// channels, with a short-lived object graph per request and one shared Mutex
// update. Everything sent through a channel escapes to the heap.
func serverHandler(requests chan *serverRequest, responses chan *serverResponse, rounds int, lock *sync.Mutex, served *int64) {
	for i := 0; i < rounds; i++ {
		request := <-requests
		seq, conn := request.seq, request.conn
		body := []int64{seq, conn, seq + 1, conn + 1, seq + 2, conn + 2, seq + 3, conn + 3}
		lock.Lock()
		*served++
		lock.Unlock()
		responses <- &serverResponse{body: body}
	}
}

func serverClient(conn int64, requests chan *serverRequest, responses chan *serverResponse, rounds int) (int64, []int64) {
	var latencies []int64
	var checksum int64
	for seq := 0; seq < rounds; seq++ {
		sent := time.Now()
		requests <- &serverRequest{conn: conn, seq: int64(seq)}
		response := <-responses
		latencies = append(latencies, int64(time.Since(sent)))
		checksum += response.body[0] + response.body[7]
	}
	return checksum, latencies
}

// Log-linear bucket: 64 sub-buckets per power of two, under 1.6% relative error.
func bucketIndex(value int64) int64 {
	shift := int64(0)
	for value >= 128 {
		value /= 2
		shift++
	}
	return shift*128 + value
}

func bucketUpper(index int64) int64 {
	upper := index%128 + 1
	for shift := index / 128; shift > 0; shift-- {
		upper *= 2
	}
	return upper - 1
}

func percentile(histogram []int64, total, percent int64) int64 {
	rank := (total*percent + 99) / 100
	seen := int64(0)
	for index, count := range histogram {
		seen += count
		if seen >= rank {
			return bucketUpper(int64(index))
		}
	}
	return -1
}

func serverSim(connections, rounds int) {
	var lock sync.Mutex
	var served int64
	checksums := make([]int64, connections)
	latencies := make([][]int64, connections)
	var before runtime.MemStats
	runtime.ReadMemStats(&before)

	fmt.Println("START")
	var wg sync.WaitGroup
	wg.Add(2 * connections)
	for i := 0; i < connections; i++ {
		requests := make(chan *serverRequest, 1)
		responses := make(chan *serverResponse, 1)
		go func() {
			defer wg.Done()
			serverHandler(requests, responses, rounds, &lock, &served)
		}()
		go func(slot int) {
			defer wg.Done()
			checksums[slot], latencies[slot] = serverClient(int64(slot), requests, responses, rounds)
		}(i)
	}
	wg.Wait()
	fmt.Println("END")

	var after runtime.MemStats
	runtime.ReadMemStats(&after)
	histogram := make([]int64, 128*48)
	var checksum, longest, samples int64
	for i := range latencies {
		checksum += checksums[i]
		for _, latency := range latencies[i] {
			histogram[bucketIndex(latency)]++
			if latency > longest {
				longest = latency
			}
			samples++
		}
	}
	fmt.Println("REQUESTS")
	fmt.Println(served)
	fmt.Println("CHECKSUM")
	fmt.Println(checksum)
	fmt.Println("LATENCY_P50_NS")
	fmt.Println(percentile(histogram, samples, 50))
	fmt.Println("LATENCY_P99_NS")
	fmt.Println(percentile(histogram, samples, 99))
	fmt.Println("LATENCY_MAX_NS")
	fmt.Println(longest)
	printGcMetrics(&before, &after)
}

func main() {
	if len(os.Args) < 3 {
		panic("usage: go-bench <idle_spawn|wake_fanout|yield_switch|ping_pong|gc_scheduler|channel_select_fan_in|spawn_join_tree|server_sim> <count> [rounds]")
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
	case "server_sim":
		serverSim(integerArg(2), integerArg(3))
	default:
		panic("unknown benchmark")
	}
}
