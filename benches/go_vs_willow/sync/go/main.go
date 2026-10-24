package main

import (
	"fmt"
	"math"
	"os"
	"runtime"
	"strconv"
)

const (
	leibnizLimit  = 100_000_000
	arrayCount    = 5_000_000
	listCount     = 1_000_000
	churnCount    = 10_000_000
	churnRing     = 1024
	mapCount      = 1_000_000
	mapKeyStride  = 2654435761
	shapeCount    = 1000
	dispatchRound = 20000
	closureCount  = 1024
	closureRounds = 20000
	stringCount   = 2_000_000
)

type node struct {
	value int64
	next  *node
}

type payload struct {
	value int64
}

type shape interface {
	area() int64
}

type square struct{ side int64 }
type rect struct{ width, height int64 }
type triangle struct{ base, height int64 }
type circle struct{ radius int64 }

func (s *square) area() int64   { return s.side * s.side }
func (r *rect) area() int64     { return r.width * r.height }
func (t *triangle) area() int64 { return t.base * t.height / 2 }
func (c *circle) area() int64   { return 3 * c.radius * c.radius }

// Exact binary increments keep the same 1,024 inputs in every language.
// Nonintegral, non-half exponent forces the general pow path.
func generalPow() float64 {
	x, sum := 1.125, 0.0
	for i := 0; i < 20_480_000; i++ {
		sum += math.Pow(x, 0.375)
		x += 0.0009765625
		if x >= 2.125 {
			x = 1.125
		}
	}
	return sum
}

func leibnizPow() float64 {
	var sum float64
	for n := 0.0; n <= float64(leibnizLimit); n++ {
		sum += math.Pow(-1.0, n) / (2.0*n + 1.0)
	}
	return 4.0 * sum
}

// Non-unit base: pow cannot return +/-1 without multiplying.
func leibnizPowNonunit() float64 {
	var sum float64
	for n := 0.0; n <= float64(leibnizLimit); n++ {
		sum += math.Pow(-1.0000001, n) / (2.0*n + 1.0)
	}
	return 4.0 * sum
}

func leibnizReduced() float64 {
	var sum float64
	sign := 1.0
	denominator := 1.0
	for i := 0; i <= leibnizLimit; i++ {
		sum += sign / denominator
		sign = -sign
		denominator += 2.0
	}
	return 4.0 * sum
}

func fib(n int64) int64 {
	if n < 2 {
		return n
	}
	return fib(n-1) + fib(n-2)
}

func arraySum() int64 {
	var values []int64
	for i := int64(0); i < arrayCount; i++ {
		values = append(values, i%1000)
	}
	var sum int64
	for _, value := range values {
		sum += value
	}
	return sum
}

// Markers match the Willow phase boundaries; validation stays outside them.
func arrayPhase(kind string) int64 {
	var values []int64
	var result int64
	if kind == "array_grow_only" {
		fmt.Println("START")
		for round := 0; round < 1000; round++ {
			var batch []int64
			for i := int64(0); i < 5000; i++ {
				batch = append(batch, i%1000)
			}
			result += int64(len(batch))
			runtime.KeepAlive(batch)
		}
		fmt.Println("END")
		return result
	}
	if kind == "array_build_only" {
		fmt.Println("START")
	}
	for i := int64(0); i < arrayCount; i++ {
		values = append(values, i%1000)
	}
	if kind == "array_build_only" {
		fmt.Println("END")
		runtime.KeepAlive(values)
		return int64(len(values))
	}
	fmt.Println("START")
	if kind == "array_read_sum_only" {
		for i := 0; i < len(values); i++ {
			result += values[i]
		}
	} else {
		for i := 0; i < len(values); i++ {
			values[i] = int64(i%1000) + 1
		}
	}
	fmt.Println("END")
	if kind == "array_write_only" {
		for i := 0; i < len(values); i++ {
			result += values[i]
		}
	}
	return result
}

// Short-lived allocation churn: every new payload evicts the one written 1,024
// allocations earlier, so each object lives briefly on the heap (the ring
// store defeats escape analysis) and the collector sees a small live set.
func objectChurn() int64 {
	ring := make([]*payload, churnRing)
	for i := range ring {
		ring[i] = &payload{}
	}
	var before runtime.MemStats
	runtime.ReadMemStats(&before)
	var sum int64
	fmt.Println("START")
	for i := int64(0); i < churnCount; i++ {
		slot := i % churnRing
		sum += ring[slot].value
		ring[slot] = &payload{value: i % 1000}
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
	fmt.Println(sum)
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
	return sum
}

// objectChurn with a three-element slice literal in place of the object.
func arrayLiteralChurn() int64 {
	ring := make([][]int64, churnRing)
	for i := range ring {
		ring[i] = []int64{0, 0, 0}
	}
	var before runtime.MemStats
	runtime.ReadMemStats(&before)
	var sum int64
	fmt.Println("START")
	for i := int64(0); i < churnCount; i++ {
		slot := i % churnRing
		sum += ring[slot][0]
		ring[slot] = []int64{i % 1000, slot, 1}
	}
	fmt.Println("END")
	var after runtime.MemStats
	runtime.ReadMemStats(&after)
	fmt.Println(sum)
	fmt.Println("GO_TOTAL_ALLOC_BYTES")
	fmt.Println(after.TotalAlloc - before.TotalAlloc)
	fmt.Println("GO_MALLOCS")
	fmt.Println(after.Mallocs - before.Mallocs)
	fmt.Println("GO_GC_CYCLES")
	fmt.Println(after.NumGC - before.NumGC)
	fmt.Println("GO_GC_PAUSE_TOTAL_NS")
	fmt.Println(after.PauseTotalNs - before.PauseTotalNs)
	return sum
}

// Insert mapCount strided keys, then look up twice as many (half miss).
func mapLookupInsert() int64 {
	table := make(map[int64]int64)
	fmt.Println("START")
	for i := int64(0); i < mapCount; i++ {
		table[i*mapKeyStride] = i
	}
	var sum int64
	for i := int64(0); i < 2*mapCount; i++ {
		if value, ok := table[i*mapKeyStride]; ok {
			sum += value
		} else {
			sum++
		}
	}
	fmt.Println("END")
	return sum
}

// Megamorphic interface call: four concrete types rotate through one call site.
func virtualDispatch() int64 {
	shapes := make([]shape, 0, shapeCount)
	for i := int64(0); i < shapeCount; i++ {
		n := i%10 + 1
		switch i % 4 {
		case 0:
			shapes = append(shapes, &square{side: n})
		case 1:
			shapes = append(shapes, &rect{width: n, height: n + 1})
		case 2:
			shapes = append(shapes, &triangle{base: n, height: n + 2})
		default:
			shapes = append(shapes, &circle{radius: n})
		}
	}
	var sum int64
	fmt.Println("START")
	for round := 0; round < dispatchRound; round++ {
		for i := 0; i < len(shapes); i++ {
			sum += shapes[i].area()
		}
	}
	fmt.Println("END")
	return sum
}

func adder(n int64) func(int64) int64 {
	return func(x int64) int64 { return x + n }
}

// Capturing closures called through a slice: the target is a run-time value.
func closureCall() int64 {
	fs := make([]func(int64) int64, 0, closureCount)
	for i := int64(0); i < closureCount; i++ {
		fs = append(fs, adder(i%7+1))
	}
	var sum int64
	fmt.Println("START")
	for round := 0; round < closureRounds; round++ {
		for i := int64(0); i < closureCount; i++ {
			sum += fs[i](i)
		}
	}
	fmt.Println("END")
	return sum
}

// Two integer-to-decimal conversions and three concatenations per string.
func stringBuild() int64 {
	var total int64
	fmt.Println("START")
	for i := int64(0); i < stringCount; i++ {
		s := "k" + strconv.FormatInt(i, 10) + ":" + strconv.FormatInt(i*7, 10)
		total += int64(len(s))
	}
	fmt.Println("END")
	return total
}

func linkedList() int64 {
	var list *node
	for i := int64(0); i < listCount; i++ {
		list = &node{value: i, next: list}
	}
	var sum int64
	for current := list; current != nil; current = current.next {
		sum += current.value
	}
	return sum
}

func main() {
	if len(os.Args) != 2 {
		panic("usage: sync-go-bench <general_pow|leibniz_pow|leibniz_pow_nonunit|leibniz_reduced|fibonacci|array_sum|linked_list|array_*_only|object_churn|array_literal_churn|map_lookup_insert|virtual_dispatch|closure_call|string_build>")
	}
	switch os.Args[1] {
	case "general_pow":
		fmt.Println(generalPow())
	case "leibniz_pow":
		fmt.Println(leibnizPow())
	case "leibniz_pow_nonunit":
		fmt.Println(leibnizPowNonunit())
	case "leibniz_reduced":
		fmt.Println(leibnizReduced())
	case "fibonacci":
		fmt.Println(fib(40))
	case "array_sum":
		fmt.Println(arraySum())
	case "array_build_only", "array_read_sum_only", "array_write_only", "array_grow_only":
		fmt.Println(arrayPhase(os.Args[1]))
	case "linked_list":
		fmt.Println(linkedList())
	case "object_churn":
		objectChurn()
	case "array_literal_churn":
		arrayLiteralChurn()
	case "map_lookup_insert":
		fmt.Println(mapLookupInsert())
	case "virtual_dispatch":
		fmt.Println(virtualDispatch())
	case "closure_call":
		fmt.Println(closureCall())
	case "string_build":
		fmt.Println(stringBuild())
	default:
		panic("unknown benchmark")
	}
}
