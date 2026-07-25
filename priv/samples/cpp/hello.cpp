#include <iostream>
#include <numeric>
#include <vector>

int main() {
    std::cout << "hello from c++\n";
    std::vector<int> v(10);
    std::iota(v.begin(), v.end(), 1);
    int sum = std::accumulate(v.begin(), v.end(), 0);
    std::cout << "sum of 1..10 = " << sum << "\n";
    return 0;
}
