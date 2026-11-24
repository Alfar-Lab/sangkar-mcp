#include <stdio.h>

struct point {
    int x;
    int y;
};

int add(int left, int right) {
    return left + right;
}

int total(struct point p) {
    return add(p.x, p.y);
}
