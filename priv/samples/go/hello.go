package main

import "fmt"

func main() {
	fmt.Println("hello from go")
	sum := 0
	for i := 1; i <= 10; i++ {
		sum += i
	}
	fmt.Printf("sum of 1..10 = %d\n", sum)
}
