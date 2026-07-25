package main

import (
	"fmt"
	"sync"
	"sync/atomic"
)

func main() {
	var counter atomic.Int64
	var wg sync.WaitGroup

	for i := 0; i < 100; i++ {
		wg.Add(1)
		go func(id int) {
			defer wg.Done()
			counter.Add(int64(id))
		}(i)
	}
	wg.Wait()

	fmt.Printf("sum 0..99 = %d (expected 4950)\n", counter.Load())
}
