#include <vector>

class Matrix {
public:
    int multiply(int factor) {
        return factor * scale(factor);
    }

    int scale(int value) {
        return value + 1;
    }
};

int compute(int factor) {
    Matrix matrix;
    return matrix.multiply(factor);
}
