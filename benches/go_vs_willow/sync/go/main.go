package main

import (
	"fmt"
	"math"
	"os"
)

const (
	leibnizLimit = 100_000_000
	arrayCount   = 5_000_000
	listCount    = 1_000_000
)

type node struct {
	value int64
	next  *node
}

func leibnizPow() float64 {
	var sum float64
	for n := 0.0; n <= float64(leibnizLimit); n++ {
		sum += math.Pow(-1.0, n) / (2.0*n + 1.0)
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
		panic("usage: sync-go-bench <leibniz_pow|leibniz_reduced|fibonacci|array_sum|linked_list>")
	}
	switch os.Args[1] {
	case "leibniz_pow":
		fmt.Println(leibnizPow())
	case "leibniz_reduced":
		fmt.Println(leibnizReduced())
	case "fibonacci":
		fmt.Println(fib(40))
	case "array_sum":
		fmt.Println(arraySum())
	case "linked_list":
		fmt.Println(linkedList())
	default:
		panic("unknown benchmark")
	}
}
