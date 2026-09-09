package main

import (
	"reflect"
	"testing"
)

func TestQuickSort(t *testing.T) {
	tests := []struct {
		name  string
		input []int
		want  []int
	}{
		{name: "nil", input: nil, want: nil},
		{name: "empty", input: []int{}, want: []int{}},
		{name: "single", input: []int{4}, want: []int{4}},
		{name: "duplicates and negatives", input: []int{3, -1, 3, 2, -1, 0}, want: []int{-1, -1, 0, 2, 3, 3}},
		{name: "already sorted", input: []int{-2, 0, 1, 7, 9}, want: []int{-2, 0, 1, 7, 9}},
		{name: "reverse sorted", input: []int{9, 7, 1, 0, -2}, want: []int{-2, 0, 1, 7, 9}},
	}

	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			QuickSort(tt.input)
			if !reflect.DeepEqual(tt.input, tt.want) {
				t.Fatalf("QuickSort() = %v, want %v", tt.input, tt.want)
			}
		})
	}
}
