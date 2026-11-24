class Shape:
    def __init__(self, name):
        self.name = name

    def area(self):
        return 0.0


class Circle(Shape):
    def __init__(self, radius):
        super().__init__("circle")
        self.radius = radius

    def area(self):
        return 3.14 * scale(self.radius)


def scale(radius):
    return radius * radius


def describe(shape):
    return shape.area() + scale(1.0)
